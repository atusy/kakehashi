# Semantic allocation attribution, 2026-07-23

This evidence records Rust `GlobalAlloc` traffic during completed
`semanticTokens/full` work-units. It is measurement-only evidence for #896; it
does not claim a latency improvement.

## Environment

- Measured server source: `089f6e72e8a189e884a02364efeb3d480e519556`
- Procedure and analyzer: this evidence directory from the PR head
- CPU: Apple M4 (`arm64`)
- OS: macOS 26.5.1
- Command Line Tools: 26.5.0.0.1777544298
- clang: Apple clang 21.0.0 (`clang-2100.1.1.101`), target
  `arm64-apple-darwin25.5.0`
- linker: `ld-1267`
- macOS SDK: 26.5
- rustc: 1.95.0 (59807616e 2026-04-14), LLVM 21.1.8
- cargo: 1.95.0

## Procedure

Build the feature-enabled profiling binary, then run one language at a time
with the synchronous driver and no competing requests. The measurement used
the project-local data directory. The measured server and the later-added
reproduction/analyzer files intentionally use two revisions:

1. Check out this PR head to obtain `prepare-data.sh`, the raw evidence, and
   `analyze.py`.
2. Build the server at the measured source SHA above in a detached worktree.
   That older source emits the retained log format without the later
   `realloc_accounting` suffix.

[`prepare-data.sh`](prepare-data.sh)
reconstructs the six workload-relevant languages from nvim-treesitter commit
`a45a920ec04cda5624f6dea0ff6454c81c3ad2d5`: that commit's `parsers.lua`
pins each grammar revision, and its query files are fetched from the same
commit. Parser-only compilation avoids the installer's mutable main-branch
query download. Hashes cover the exact grammar inputs (`grammar.json`,
`parser.c`, and scanners) plus queries, rather than nondeterministic linked
Mach-O bytes, and are verified by
[`parser-artifacts.sha256`](parser-artifacts.sha256):

```sh
measurement_source=089f6e72e8a189e884a02364efeb3d480e519556
measurement_root="$(mktemp -d "${TMPDIR:-/tmp}/kakehashi-measurement.XXXXXX")"
measurement_worktree="$measurement_root/source"
measurement_target="$measurement_root/target"
git worktree add --detach "$measurement_worktree" "$measurement_source"
cargo build \
  --manifest-path "$measurement_worktree/Cargo.toml" \
  --target-dir "$measurement_target" \
  --profile profiling --features allocation-profile --bin kakehashi
measurement_bin="$measurement_target/profiling/kakehashi"
KAKEHASHI_MEASUREMENT_BIN="$measurement_bin" \
  benches/results/allocation-attribution-2026-07-23/prepare-data.sh

RUST_LOG=kakehashi::semantic=debug \
  python3 "$measurement_worktree/benches/profile/drive.py" \
    --bin "$measurement_bin" \
    --data-dir "$PWD/deps/test/kakehashi" \
    --lang markdown --size 150 --requests 36 --edits 1 \
    2> /tmp/kakehashi-allocation-markdown-final.log

RUST_LOG=kakehashi::semantic=debug \
  python3 "$measurement_worktree/benches/profile/drive.py" \
    --bin "$measurement_bin" \
    --data-dir "$PWD/deps/test/kakehashi" \
    --lang rust --size 150 --requests 36 --edits 1 \
    2> /tmp/kakehashi-allocation-rust-final.log
```

The 36 allocation log lines, with only logger timestamps/prefixes removed, are
retained in [`markdown.log`](markdown.log) and [`rust.log`](rust.log).
[`markdown-all.tsv`](markdown-all.tsv) and [`rust-all.tsv`](rust-all.tsv)
preserve their parsed fields. The first 6 completed records are warmups; the
remaining 30 are renumbered into [`markdown.tsv`](markdown.tsv) and
[`rust.tsv`](rust.tsv). Verify both extraction steps and print the summary
with:

```sh
evidence=benches/results/allocation-attribution-2026-07-23
python3 "$evidence/analyze.py" \
  "$evidence/markdown.log" \
  "$evidence/markdown-all.tsv" "$evidence/markdown.tsv"
python3 "$evidence/analyze.py" \
  "$evidence/rust.log" \
  "$evidence/rust-all.tsv" "$evidence/rust.tsv"
```

The script uses the nearest-rank percentile convention
(`sorted[ceil(p * n) - 1]`). The complete original logs also contain unrelated
driver and phase timing records and are not committed; their hashes preserve
the provenance of the sanitized allocation-only logs:

Full-log SHA-256:

- Markdown:
  `aa78016797d83f53d96c5be187efd4055f864a0e0e42fe5112c208bb4fa182cd`
- Rust:
  `77792e63729b19eeb7602cba9c49be2b05868b795b9a8dbf2dc76bfe25169618`

## Results

| Language | Metric | n | Mean | p50 | p90 | Min | Max |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Markdown | allocation events | 30 | 19,015.6 | 20,022 | 22,883 | 5,269 | 47,317 |
| Markdown | requested allocated bytes | 30 | 7,983,801.3 | 8,027,139 | 8,297,091 | 7,349,392 | 8,774,584 |
| Rust | allocation events | 30 | 87,132.0 | 94,424 | 108,393 | 58,245 | 108,625 |
| Rust | requested allocated bytes | 30 | 24,305,204.4 | 24,362,260 | 25,268,752 | 23,196,788 | 25,269,509 |

The Rust workload shows especially high realloc-inclusive requested-capacity
traffic. This identifies artifact construction and chunk/buffer reuse as
measurement candidates for #896; this evidence alone does not determine
retained growth or predict the latency effect of either change.

## Scope and limitations

- The counters cover Rust `GlobalAlloc` calls in the whole process during the
  semantic work-unit window. Native allocations such as Tree-sitter
  `ts_malloc` are excluded.
- A successful `realloc` contributes one full old-layout deallocation and one
  full requested new-size allocation even when it happens in place. Counts are
  therefore allocation events, not distinct pointers, and byte totals are
  requested-capacity traffic, not bytes physically copied.
- The process-wide attribution is meaningful here because the synchronous
  driver issued no competing requests. Rayon allocations through
  `GlobalAlloc` remain included.
- The four counters use independent atomic snapshots. Boundary observations
  are approximate, so the aggregate of repeated records is the intended unit
  of evidence; derived net-live values are intentionally not reported.
- `allocation-profile` adds atomic counter overhead. Therefore this run makes
  no latency claim; latency comparisons must use builds without that feature.
- Work whose cancellation is observed by the final compute checkpoint has no
  completed allocation record. A cancellation racing after that checkpoint may
  still make the caller discard a result whose record was already emitted.
