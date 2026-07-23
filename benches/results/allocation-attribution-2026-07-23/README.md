# Semantic allocation attribution, 2026-07-23

This evidence records Rust `GlobalAlloc` traffic during completed
`semanticTokens/full` work-units. It is measurement-only evidence for #896; it
does not claim a latency improvement.

## Environment

- Source: `089f6e72e8a189e884a02364efeb3d480e519556`
- CPU: Apple M4 (`arm64`)
- OS: macOS 26.5.1
- rustc: 1.95.0 (59807616e 2026-04-14), LLVM 21.1.8
- cargo: 1.95.0

## Procedure

Build the feature-enabled profiling binary, then run one language at a time
with the synchronous driver and no competing requests:

```sh
cargo build --profile profiling --features allocation-profile --bin kakehashi

RUST_LOG=kakehashi::semantic=debug \
  python3 benches/profile/drive.py \
    --bin ./target/profiling/kakehashi \
    --lang markdown --size 150 --requests 36 --edits 1 \
    2> /tmp/kakehashi-allocation-markdown-final.log

RUST_LOG=kakehashi::semantic=debug \
  python3 benches/profile/drive.py \
    --bin ./target/profiling/kakehashi \
    --lang rust --size 150 --requests 36 --edits 1 \
    2> /tmp/kakehashi-allocation-rust-final.log
```

The first 6 completed records in each log are warmups and were discarded. The
30 retained records are preserved verbatim as columns in
[`markdown.tsv`](markdown.tsv) and [`rust.tsv`](rust.tsv).

Full-log SHA-256:

- Markdown:
  `aa78016797d83f53d96c5be187efd4055f864a0e0e42fe5112c208bb4fa182cd`
- Rust:
  `77792e63729b19eeb7602cba9c49be2b05868b795b9a8dbf2dc76bfe25169618`

## Results

| Language | Metric | n | Mean | p50 | p90 | Min | Max |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Markdown | allocations | 30 | 19,015.6 | 20,022 | 22,883 | 5,269 | 47,317 |
| Markdown | allocated bytes | 30 | 7,983,801.3 | 8,027,139 | 8,297,091 | 7,349,392 | 8,774,584 |
| Rust | allocations | 30 | 87,132.0 | 94,424 | 108,393 | 58,245 | 108,625 |
| Rust | allocated bytes | 30 | 24,305,204.4 | 24,362,260 | 25,268,752 | 23,196,788 | 25,269,509 |

The Rust workload shows especially high allocation traffic. Together with the
much smaller retained-heap growth seen in the complementary exact-PID heap
profile, this supports investigating request-owned artifact construction and
chunk/buffer reuse in #896 rather than treating retained growth as the primary
problem.

## Scope and limitations

- The counters cover Rust `GlobalAlloc` calls in the whole process during the
  semantic work-unit window. Native allocations such as Tree-sitter
  `ts_malloc` are excluded.
- The process-wide attribution is meaningful here because the synchronous
  driver issued no competing requests. Rayon allocations through
  `GlobalAlloc` remain included.
- The four counters use independent atomic snapshots. Boundary observations
  are approximate, so the aggregate of repeated records is the intended unit
  of evidence; derived net-live values are intentionally not reported.
- `allocation-profile` adds atomic counter overhead. Therefore this run makes
  no latency claim; latency comparisons must use builds without that feature.
- Cancelled work has no completed allocation record.
