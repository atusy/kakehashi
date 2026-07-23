# Semantic artifact seam benchmark

This run checks that introducing the immutable semantic-artifact boundary does
not regress the freshly computed semantic-token paths. It does not claim a
performance improvement.

## Contract

- A: `refactor/semantic-sync-core` at
  `265d38b0ac95822b17e5cec19c5e226d3d034bb9`
- B: `benchmark/semantic-artifact-seam-2026-07-23` at
  `1805e61907b6f2b6ce4718b5145d131a174c4a2e`
- Harness: `benchmark/semantic-lazy-content-lines-harness-2026-07-23` at
  `553b901bbf421e3851886c2a3e94b16adca6c34a`
- Four alternating AB/BA pairs
- Six warm-up and 30 retained iterations per scenario
- Lower is better; percentages are paired B-versus-A median deltas

The fixture manifest records the installed language assets used by the run.
Parser libraries and query files are not archived in this evidence directory.

## Result

The primary freshly computed paths showed no repeatable regression:

| Scenario | Paired median | Faster/slower pairs |
| --- | ---: | ---: |
| Rust typing delta | -0.20% | 2 / 2 |
| Rust typing burst | +0.26% | 2 / 2 |
| Rust cancellation burst | -1.82% | 2 / 2 |
| Markdown typing delta | +3.01% | 1 / 3 |
| Markdown typing burst | +0.38% | 2 / 2 |

The Markdown delta result had mixed signs and a wide -6.15% to +6.55% range.
Cache-hit controls were likewise noisy even though they do not traverse the new
artifact construction path. The result therefore supports non-regression, not
a speedup claim.

`manifest.json` attests the exact commits, harness sources, binaries, fixture
manifest, raw pair files, stdout captures, and aggregate summary.
