# Single Tree Worker Stage 5 Measurement

## Scope

Stage 5 keeps the worker in shadow mode and adds content-addressed grammar
identity, planned replacement, recovery-window accounting, and generation-gated
circuit-breaker probes. This measurement checks that computing a parser digest
once per configuration generation does not move file hashing onto the
`didChange` hot path or regress the paired Stage 3 shadow workload.

## Method and provenance

The committed result is
`benches/profile/results/single_worker_stage5_shadow_2026-07-19.json`. An
isolated clean-source build attested commit
`f5471d6b3a43386f0da8453f7713729fed344993`; the release binary SHA-256 is
`6d4141aab575dd044c22bd75b532108b4f8f0671c3721cd3f6ec4b1f870ef271`.
The parser/query corpus is pinned to nvim-treesitter revision
`4916d6592ede8c07973490d9322f187e07dfefac`.

The Stage 3 collector ran four alternating paired batches. Each variant opened
one 200-line Rust document and performed 200 ranged edits, each followed by a
synchronous semantic-token request. Shadow mode used four worker threads. The
comparison drain requires all 201 document versions to converge.

## Results

| Batch and order | Control wall ms | Shadow wall ms | Wall delta | Control p50 ms | Shadow p50 ms | p50 delta |
|---|---:|---:|---:|---:|---:|---:|
| 1, control then shadow | 9543.1 | 9340.5 | -2.12% | 30.8 | 30.4 | -1.30% |
| 2, shadow then control | 8870.8 | 9144.6 | +3.09% | 29.2 | 30.4 | +4.11% |
| 3, control then shadow | 9033.1 | 9983.2 | +10.52% | 30.2 | 33.3 | +10.26% |
| 4, shadow then control | 9017.5 | 9748.0 | +8.10% | 30.3 | 32.5 | +7.26% |

The median paired wall delta is **+5.60%** and the mean is **+4.90%**. The
median paired p50 delta is **+5.69%** and the mean is **+5.08%**. Every shadow
batch matched all 201 versions with zero mismatches and zero pending results.

For context, the earlier two-batch Stage 3 result reported +6.27% and +7.92%
wall deltas and +10.58% and +11.02% p50 deltas. The source commits differ, and
the four Stage 5 batches still span -2.12% to +10.52%, so this is evidence of no
observable regression rather than a claim that content identity improved parse
speed.

## Interpretation

`worker_grammar_descriptor` caches the SHA-256 by canonical parser path and
configuration generation. The 200 edits in each batch therefore reuse one
digest; a configuration reload invalidates the cache and re-hashes once. The
wire protocol carries the digest, but the added bytes are negligible beside
the full-text initial sync and do not recur in incremental edit payloads.

The measured overhead remains duplicate shadow parsing and scheduler
contention. Authoritative cutover must remove the parent-side duplicate parse
and rerun the same paired workload before making a production performance
claim.

## Reproduction

```sh
python3 benches/profile/attest_worker_binary.py \
  --checkout . \
  --output benches/profile/results/single_worker_stage5_binary_attestation_2026-07-19.json

python3 benches/profile/collect_worker_shadow.py \
  --bin ./target/release/kakehashi \
  --binary-attestation benches/profile/results/single_worker_stage5_binary_attestation_2026-07-19.json \
  --data-dir "$DATA_DIR" \
  --nvim-treesitter-checkout "$NVIM_TREESITTER_CHECKOUT" \
  --batches 4 --worker-threads 4 \
  --output benches/profile/results/single_worker_stage5_shadow_2026-07-19.json
```
