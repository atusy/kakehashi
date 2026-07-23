# Same-snapshot semantic artifact sharing control rerun

This independent collection repeats the controls whose first converged run had
the largest variance. It uses the same refs and measurement contract as the
full control collection: four alternating AB/BA pairs, six warm-up iterations,
30 retained iterations, and production server features only.

- A: `benchmark/semantic-sharing-baseline-2026-07-24` at
  `16b857a74f637e7ac20c4b503b670cd3caca1738`
- B and harness:
  `benchmark/semantic-sharing-candidate-converged-2026-07-24` at
  `d2ea0c73eddf3b59f452e94e5183af0ec3e00b97`

The candidate is the converged Stage 3 runtime plus the #911 harness, with #911
intended to merge first. The fixture manifest contains only installed asset
paths and digests; parser libraries and query bodies are not archived.

## Result

| Scenario | Paired median | Pair range |
| --- | ---: | ---: |
| Rust typing burst | -8.41% | -25.74% to +27.04% |
| Rust cancellation burst | +5.82% | -6.23% to +80.72% |
| Markdown large cache hit | +12.30% | -37.46% to +52.65% |
| Unicode Rust cache hit | -1.14% | -14.09% to +12.57% |

Rust typing burst changed sign relative to the first collection and alternated
between improvement and regression by pair. Cancellation burst was near neutral
after the unusually fast first baseline pair. These results do not support a
stable regression claim, but their variance also prevents claiming a control
speedup. The Stage 3 speedup claim is therefore limited to the separately
measured same-snapshot fan-out workload.

`manifest.json` retains the exact source, binary, fixture, raw-pair, and summary
attestations for this rerun.
