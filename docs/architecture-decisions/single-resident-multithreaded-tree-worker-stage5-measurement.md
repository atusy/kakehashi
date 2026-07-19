# Single Tree Worker Stage 5 Measurement

## Scope

Stage 5 keeps the worker in shadow mode and adds content-addressed grammar
identity, planned replacement, recovery-window accounting, and generation-gated
circuit-breaker probes. This measurement checks that computing a parser digest
once per configuration generation does not move file hashing onto the
`didChange` hot path or regress the paired Stage 3 shadow workload.

## Method and provenance

The committed result is
`benches/profile/results/single_worker_stage5_shadow_2026-07-20.json`. An
isolated clean-source build attested commit
`691c877f2017e9f1885b8dff005b02be3e603317`; the release binary SHA-256 is
`dc504feebbf2ad6e7116a07708aaddfddbd44d70b1282ffd355871977b067041`.
The parser/query corpus is pinned to nvim-treesitter revision
`4916d6592ede8c07973490d9322f187e07dfefac`.

The Stage 3 collector ran four alternating paired batches. Each variant opened
one 200-line Rust document and performed 200 ranged edits, each followed by a
synchronous semantic-token request. Shadow mode used four worker threads. The
comparison drain requires all 201 document versions to converge.

## Results

| Batch and order | Control wall ms | Shadow wall ms | Wall delta | Control p50 ms | Shadow p50 ms | p50 delta |
|---|---:|---:|---:|---:|---:|---:|
| 1, control then shadow | 11145.6 | 12159.2 | +9.09% | 38.3 | 41.8 | +9.14% |
| 2, shadow then control | 10641.4 | 11896.7 | +11.80% | 35.9 | 40.6 | +13.09% |
| 3, control then shadow | 10619.8 | 11725.6 | +10.41% | 35.9 | 40.0 | +11.42% |
| 4, shadow then control | 10623.9 | 11751.3 | +10.61% | 35.9 | 40.1 | +11.70% |

The median paired wall delta is **+10.51%** and the mean is **+10.48%**. The
median paired p50 delta is **+11.56%** and the mean is **+11.34%**. Every shadow
batch matched all 201 versions with zero mismatches and zero pending results.

For context, the pre-review Stage 5 run at `f5471d6b3` reported a +5.60% median
wall delta and +5.69% median p50 delta, while the earlier two-batch Stage 3 run
reported +6.27% and +7.92% wall deltas and +10.58% and +11.02% p50 deltas. The
latest four batches are more tightly grouped at +9.09% to +11.80%, but both
control and shadow absolute times are slower than the older run. The source and
ambient conditions differ, so the increase is not attributed to one code path
without profiling. The current result remains below the ADR's +15% median
shadow-overhead gate.

## Interpretation

`worker_grammar_descriptor` imports a parser twice under a single-flight lock,
accepts it only when both byte streams and source metadata agree, and caches the
private content-addressed artifact by settings epoch. The 200 edits therefore
reuse one descriptor. The parent registry retains the latest full document for
crash recovery, but the worker wire path remains incremental after initial
sync.

The measured overhead remains duplicate shadow parsing and scheduler
contention. Authoritative cutover must remove the parent-side duplicate parse
and rerun the same paired workload before making a production performance
claim.

## Reproduction

```sh
python3 benches/profile/attest_worker_binary.py \
  --checkout . \
  --output benches/profile/results/single_worker_stage5_binary_attestation_2026-07-20.json

python3 benches/profile/collect_worker_shadow.py \
  --bin ./target/release/kakehashi \
  --binary-attestation benches/profile/results/single_worker_stage5_binary_attestation_2026-07-20.json \
  --data-dir "$DATA_DIR" \
  --nvim-treesitter-checkout "$NVIM_TREESITTER_CHECKOUT" \
  --batches 4 --worker-threads 4 \
  --output benches/profile/results/single_worker_stage5_shadow_2026-07-20.json
```
