# Semantic artifact seam benchmark

This run checks that introducing the immutable semantic-artifact boundary does
not regress the freshly computed semantic-token paths. It does not claim a
performance improvement.

## Contract

- A: `refactor/semantic-sync-core` at
  `265d38b0ac95822b17e5cec19c5e226d3d034bb9`
- B: `benchmark/semantic-artifact-seam-converged-2026-07-23` at
  `8b966d16f5a3a56c985a71d16b91e2e1fe5ae53f`
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
| Rust typing delta | +1.13% | 2 / 2 |
| Rust typing burst | +0.15% | 2 / 2 |
| Rust cancellation burst | +0.54% | 2 / 2 |
| Markdown typing delta | -0.61% | 2 / 2 |
| Markdown typing burst | +3.49% | 2 / 2 |

Every primary scenario had mixed pair signs. Controls that do not traverse
artifact construction were noisier: Markdown large cache hit had a +23.80%
paired median with a -38.74% to +62.22% range, while Unicode cache hit ranged
from -0.77% to +46.36%. The result therefore supports non-regression only, not a
speedup claim.

`manifest.json` attests the exact commits, harness sources, binaries, fixture
manifest, raw pair files, stdout captures, and aggregate summary.
