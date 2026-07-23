#!/bin/sh
# Recreate the macOS parser/query data used by this measurement.
set -eu

data_dir=deps/test/kakehashi
nvim_treesitter_revision=a45a920ec04cda5624f6dea0ff6454c81c3ad2d5
nvim_treesitter_raw="https://raw.githubusercontent.com/nvim-treesitter/nvim-treesitter/$nvim_treesitter_revision"
evidence_dir=benches/results/allocation-attribution-2026-07-23
measurement_bin=${KAKEHASHI_MEASUREMENT_BIN:?set to the profiling binary built from the measured source}
source_root="$(mktemp -d "${TMPDIR:-/tmp}/kakehashi-parser-source.XXXXXX")"
pinned_source_dir="$data_dir/pinned-source"

cleanup_source() {
  trap - EXIT HUP INT TERM
  rm -r "$source_root"
}
trap cleanup_source EXIT
trap 'cleanup_source; exit 129' HUP
trap 'cleanup_source; exit 130' INT
trap 'cleanup_source; exit 143' TERM

[ -x "$measurement_bin" ] || {
  printf 'measurement binary is not executable: %s\n' "$measurement_bin" >&2
  exit 1
}

rm -r "$pinned_source_dir" 2>/dev/null || true
mkdir -p "$data_dir/cache" "$data_dir/parser" "$pinned_source_dir"
for language in comment lua markdown markdown_inline python rust; do
  rm -r "$data_dir/queries/$language" 2>/dev/null || true
  mkdir -p "$data_dir/queries/$language"
done
curl -fL \
  "$nvim_treesitter_raw/lua/nvim-treesitter/parsers.lua" \
  -o "$data_dir/cache/parsers.lua"

compile_parser() {
  language=$1
  repository=$2
  revision=$3
  location=$4
  checkout="$source_root/$language"

  git init -q "$checkout"
  git -C "$checkout" remote add origin "$repository"
  git -C "$checkout" -c http.followRedirects=false \
    -c protocol.allow=never -c protocol.https.allow=always \
    fetch --depth 1 origin -- "$revision"
  git -C "$checkout" checkout -q --detach FETCH_HEAD
  mkdir -p "$pinned_source_dir/$language"
  cp -R "$checkout/$location/src/." "$pinned_source_dir/$language"
  "$measurement_bin" __compile-parser \
    "$checkout/$location" "$data_dir/parser/$language.dylib"
}

# Revisions and locations come from the pinned parsers.lua above. Calling the
# measured binary's parser-only internal compiler avoids the normal install
# command's mutable main-branch query download.
compile_parser comment https://github.com/stsewd/tree-sitter-comment \
  66272d2b6c73fb61157541b69dd0a7ce7b42a5ad .
compile_parser lua https://github.com/tree-sitter-grammars/tree-sitter-lua \
  10fe0054734eec83049514ea2e718b2a56acd0c9 .
compile_parser markdown https://github.com/tree-sitter-grammars/tree-sitter-markdown \
  c3570720f7f7bbad22fe96603f106276618e0cf5 tree-sitter-markdown
compile_parser markdown_inline https://github.com/tree-sitter-grammars/tree-sitter-markdown \
  c3570720f7f7bbad22fe96603f106276618e0cf5 tree-sitter-markdown-inline
compile_parser python https://github.com/tree-sitter/tree-sitter-python \
  293fdc02038ee2bf0e2e206711b69c90ac0d413f .
compile_parser rust https://github.com/tree-sitter/tree-sitter-rust \
  77a3747266f4d621d0757825e6b11edcbf991ca5 .

# The installer normally downloads queries from main. Replace every query that
# these workloads can load with the files from the same pinned nvim-treesitter
# revision.
for query_path in \
  comment/highlights.scm \
  lua/highlights.scm lua/injections.scm \
  markdown/highlights.scm markdown/injections.scm \
  markdown_inline/highlights.scm markdown_inline/injections.scm \
  python/highlights.scm python/injections.scm \
  rust/highlights.scm rust/injections.scm
do
  curl -fL \
    "$nvim_treesitter_raw/runtime/queries/$query_path" \
    -o "$data_dir/queries/$query_path"
done

shasum -a 256 -c "$evidence_dir/parser-artifacts.sha256"
