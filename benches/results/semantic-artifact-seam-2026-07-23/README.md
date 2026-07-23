# Semantic artifact seam benchmark

This run checks that introducing the immutable semantic-artifact boundary does
not regress the freshly computed semantic-token paths. It does not claim a
performance improvement.

## Contract

- A: `refactor/semantic-sync-core` at
  `265d38b0ac95822b17e5cec19c5e226d3d034bb9`
- B: `benchmark/semantic-artifact-seam-2026-07-23` at
  `518c1868e7c955d5241b3a5acaadf5d0617981b9`
- Harness: `benchmark/semantic-lazy-content-lines-harness-2026-07-23` at
  `553b901bbf421e3851886c2a3e94b16adca6c34a`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per scenario
- Lower is better; percentages are paired B-versus-A median deltas

The fixture manifest records the installed language assets used by the run.
Parser libraries and query files are not archived in this evidence directory.

## Result

| Scenario | Paired median | Faster/slower pairs |
| --- | ---: | ---: |
| Rust typing delta | -4.26% | 4 / 0 |
| Rust typing burst | -8.30% | 4 / 0 |
| Rust cancellation burst | +4.04% | 1 / 3 |
| Markdown typing delta | +7.05% | 1 / 3 |
| Markdown typing burst | -1.28% | 3 / 1 |

The run had substantial pair-to-pair noise. Cancellation ranged from -4.05% to
+70.72%, Markdown delta ranged from -10.89% to +17.07%, and cache-hit controls
that do not traverse artifact construction also moved by a paired median of up
to +4.88%. The Rust improvements are therefore not claimed as a speedup, while
the mixed signs and noisy controls provide no repeatable primary-path regression
signal attributable to the seam.

`manifest.json` attests the exact commits, harness sources, binaries, fixture
manifest, raw pair files, stdout captures, and aggregate summary.
