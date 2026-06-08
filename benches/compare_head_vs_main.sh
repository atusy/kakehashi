#!/usr/bin/env bash
#
# A/B-benchmark the semantic-tokens hot path of HEAD against a baseline ref
# (default: origin/main).
#
# How it works: it builds the `kakehashi` *binary* from both revisions, then runs
# the single benchmark harness (benches/semantic_tokens.rs, always from HEAD)
# against both binaries. Because the harness drives the server over LSP as a
# separate process, the same harness can benchmark any binary — so the baseline
# revision never needs to contain this benchmark.
#
# Usage:
#   benches/compare_head_vs_main.sh [BASELINE_REF]
#
# Env:
#   KAKEHASHI_BENCH_ITERS / KAKEHASHI_BENCH_WARMUP  forwarded to the harness.
set -euo pipefail

BASELINE_REF="${1:-origin/main}"
REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

WORK="$(mktemp -d)"
WORKTREE="$WORK/baseline-src"
HEAD_BIN="$WORK/kakehashi-head"
BASE_BIN="$WORK/kakehashi-baseline"

cleanup() {
  git worktree remove --force "$WORKTREE" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "==> Building HEAD binary"
cargo build --release --bin kakehashi
cp target/release/kakehashi "$HEAD_BIN"

echo "==> Building ${BASELINE_REF} binary in a detached worktree"
git worktree add --detach "$WORKTREE" "$BASELINE_REF"
# Separate target dir so the baseline build can't clash with HEAD's artifacts.
( cd "$WORKTREE" && CARGO_TARGET_DIR="$WORK/baseline-target" cargo build --release --bin kakehashi )
cp "$WORK/baseline-target/release/kakehashi" "$BASE_BIN"

echo "==> Running A/B benchmark (A=${BASELINE_REF}, B=HEAD)"
# A = baseline, B = HEAD, so the reported Δ (B vs A) is HEAD relative to baseline:
# a negative Δ means HEAD is faster.
# The harness needs --features e2e to reach install::test_support for parser
# setup. The benchmarked binaries were built separately above without it.
KAKEHASHI_BENCH_BIN_A="$BASE_BIN" KAKEHASHI_BENCH_LABEL_A="baseline" \
KAKEHASHI_BENCH_BIN_B="$HEAD_BIN" KAKEHASHI_BENCH_LABEL_B="HEAD" \
  cargo bench --bench semantic_tokens --features e2e
