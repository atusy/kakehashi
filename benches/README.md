# Benchmarks

`semantic_tokens.rs` drives release server binaries over LSP. For exploratory
single-binary runs, invoke it with `cargo bench --bench semantic_tokens
--features e2e`. For evidence used in a performance decision, use the paired
collector instead:

```sh
python3 benches/collect_semantic_pairs.py \
  origin/main candidate-ref benches/results/main-vs-candidate
```

The collector currently requires POSIX, Python 3.10 or newer, a clean committed
checkout, Git, Cargo, Rust, and the platform C toolchain needed to compile
tree-sitter parsers. Its default contract runs four alternating `AB`, `BA`,
`AB`, `BA` pairs with six warmups and 30 retained samples.

Builds and timed servers run with an allowlisted environment. The collector
uses isolated `HOME`, `TMPDIR`, and `CARGO_HOME` directories; ambient Cargo,
Rust, logging, XDG, and `KAKEHASHI_*` overrides are not inherited. Intentional
server variables must be explicit and are recorded:

```sh
python3 benches/collect_semantic_pairs.py \
  origin/main candidate-ref benches/results/main-vs-candidate \
  --server-env KAKEHASHI_TREE_WORKER_MODE=legacy
```

The output path must be absent or empty. Collection happens in a sibling
staging directory and is published atomically only after every pair,
attestation, and raw-contract check succeeds. A failed run removes its staging
artifacts and reports any exact worktree cleanup failure, so the same output
path can be retried.

The resulting `manifest.json` binds source commits, binary and harness hashes,
the read-only parser/query fixture (whose exact contents are archived alongside
the manifest), effective build/runtime environment,
toolchain versions, raw sample hashes, and summary hash. `summary.json` reports
the median and range of paired median deltas. Per-run p95 is descriptive only.

If a measured source is not retained by the repository's main history, give it
a durable tag before collection and use that tag as the collector ref. This
keeps every source commit named by archived evidence fetchable after temporary
development branches are deleted. Before merging archived evidence, also retain
the recorded harness commit with a durable tag and add that tag as
`source.harness_ref` in the published manifest.
