# Stable empty-delta snapshot promotion

This run compares baseline `265d38b0ac95822b17e5cec19c5e226d3d034bb9`
with candidate `e77599ec2e937216bf9c028d330ae6dc4983cac3`.
Lower paired delta is better. It used eight alternating A/B pairs, 12 warmups,
and 30 retained samples per scenario.

## Result

| Scenario | Paired median | Pair range |
| --- | ---: | ---: |
| Rust stable-edit follow-up delta | -45.30% | -52.75% .. -24.83% |
| Markdown stable-edit follow-up delta | -11.24% | -27.73% .. +11.15% |
| Rust typing delta control | -6.34% | -13.07% .. -1.29% |
| Markdown typing delta control | +0.25% | -1.04% .. +33.56% |
| Rust full-cache-hit control | +4.98% | -10.37% .. +39.09% |
| Markdown full-cache-hit control | +7.10% | -7.93% .. +20.73% |

The candidate establishes a repeatable improvement only for the Rust
stable-edit follow-up path: all eight pairs improved. The Markdown target
improved in six of eight pairs but crossed zero, so it is directional evidence
rather than a repeatable-win claim.

The full-cache-hit controls have sub-millisecond-to-low-millisecond absolute
latency and wide, sign-changing ranges. They do not support either a regression
or a non-regression claim. Markdown typing is likewise inconclusive. Rust typing
improved in every pair, but it is a control rather than the changed cache path,
so the performance claim remains limited to the intended stable-edit follow-up
path.

Every stable-edit sample validates that both the first response after the
unique edit and the same-snapshot follow-up are empty deltas preserving the
client's result ID. Focused handler tests separately pin that the first response
promotes the baseline to the edited snapshot identity after the final
generation, snapshot, and request-currency checks.
