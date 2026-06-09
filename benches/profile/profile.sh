#!/usr/bin/env bash
#
# Profile the semanticTokens/full hot path and render a flamegraph.
#
# Drives the server synchronously (one request at a time, waiting for each
# response) so it does real work — a piped static session would be answered with
# `-32800 Canceled` because the server coalesces superseded requests. Samples
# with samply (no sudo on macOS), symbolicates kakehashi frames offline with
# atos, and renders an SVG with inferno-flamegraph.
#
# Requires: samply + inferno (`cargo install samply inferno`), python3, dsymutil.
#
# Usage:
#   benches/profile/profile.sh [--lang rust|markdown] [--size N] [--requests N]
set -euo pipefail

LANG_ARG=rust SIZE=150 REQUESTS=150
while [ $# -gt 0 ]; do
  case "$1" in
    --lang) LANG_ARG="$2"; shift 2;;
    --size) SIZE="$2"; shift 2;;
    --requests) REQUESTS="$2"; shift 2;;
    *) echo "unknown arg: $1" >&2; exit 1;;
  esac
done

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"
HERE="benches/profile"
BIN="target/profiling/kakehashi"
DWARF="target/profiling/kakehashi.dSYM/Contents/Resources/DWARF/kakehashi"
ARCH="$(uname -m)"
OUT="/tmp/kakehashi-profile"
mkdir -p "$OUT"

echo "==> Building profiling binary (optimized + debug symbols)"
cargo build --profile profiling --bin kakehashi
echo "==> Generating dSYM for symbolication"
dsymutil "$BIN"

echo "==> Recording with samply (lang=$LANG_ARG size=$SIZE requests=$REQUESTS)"
samply record -s -o "$OUT/profile.json.gz" \
  -- python3 "$HERE/drive.py" --bin "./$BIN" --lang "$LANG_ARG" --size "$SIZE" --requests "$REQUESTS"

echo "==> Analyzing + writing collapsed stacks"
python3 "$HERE/analyze.py" "$OUT/profile.json.gz" --dsym "$DWARF" --arch "$ARCH" \
  --folded "$OUT/profile.folded"

echo "==> Rendering flamegraph"
inferno-flamegraph --title "kakehashi semanticTokens/full ($LANG_ARG)" \
  "$OUT/profile.folded" > "$OUT/flamegraph.svg"
echo "flamegraph -> $OUT/flamegraph.svg"
