#!/bin/sh
# Recreate the macOS parser/query data used by this measurement.
set -eu

data_dir=deps/test/kakehashi
nvim_treesitter_revision=a45a920ec04cda5624f6dea0ff6454c81c3ad2d5
nvim_treesitter_raw="https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/$nvim_treesitter_revision"
evidence_dir=benches/results/allocation-attribution-2026-07-23

mkdir -p "$data_dir/cache"
curl -fL \
  "$nvim_treesitter_raw/lua/nvim-treesitter/parsers.lua" \
  -o "$data_dir/cache/parsers.lua"

# parsers.lua pins the grammar revisions. The measurement workloads load these
# six languages: Markdown and its three fenced languages, Rust, and comments.
for language in comment lua markdown markdown_inline python rust; do
  cargo run --quiet --profile profiling -- \
    language install --data-dir "$data_dir" --force "$language"
done

# The installer normally downloads queries from main. Replace every query that
# these workloads can load with the files from the same pinned nvim-treesitter
# revision.
for path in \
  comment/highlights.scm \
  lua/highlights.scm lua/injections.scm \
  markdown/highlights.scm markdown/injections.scm \
  markdown_inline/highlights.scm markdown_inline/injections.scm \
  python/highlights.scm python/injections.scm \
  rust/highlights.scm rust/injections.scm
do
  curl -fL \
    "$nvim_treesitter_raw/runtime/queries/$path" \
    -o "$data_dir/queries/$path"
done

shasum -a 256 -c "$evidence_dir/parser-artifacts.sha256"
