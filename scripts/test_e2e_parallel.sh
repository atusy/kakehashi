#!/usr/bin/env bash
#
# Run the E2E / integration test binaries CONCURRENTLY.
#
# `cargo test` runs integration-test binaries one at a time (it only parallelizes
# the test functions *within* a single binary). Each of our E2E binaries spends
# most of its wall time waiting on a spawned `kakehashi` server (and, for the
# bridge tests, on `lua-language-server`), so a sequential run leaves a many-core
# machine almost idle — the full suite takes ~95s here at ~1.2x core utilization.
#
# This script builds the binaries once, then runs them in a bounded parallel
# pool. The bound matters: oversubscribing (e.g. 30 concurrent test threads on 10
# cores) makes timing-sensitive tests and the lua-ls poll loops thrash and run
# SLOWER than sequential. A total concurrency of ~1.5x cores is the sweet spot
# (measured here: ~31s, a 3x speedup, with identical pass counts).
#
# Safe to run concurrently because each spawned server is isolated:
#   - KAKEHASHI_DATA_DIR  : shared, read-only after install (parsers/queries)
#   - XDG_CONFIG_HOME     : per-process temp dir (no user-config bleed)
#   - KAKEHASHI_STATE_DIR : per-process temp dir (crash-recovery state, set by
#                           the test harness so processes don't poison each other)
#
# Usage:
#   scripts/test_e2e_parallel.sh
#   E2E_JOBS=8 E2E_INNER_THREADS=2 scripts/test_e2e_parallel.sh
#
# Env:
#   E2E_JOBS           number of test BINARIES to run at once (default: ~0.75x cores)
#   E2E_INNER_THREADS  test threads WITHIN each binary       (default: 2)
#                      → total concurrency ≈ E2E_JOBS * E2E_INNER_THREADS
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

CORES=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)
DEFAULT_JOBS=$(( (CORES * 3 + 3) / 4 ))
[ "$DEFAULT_JOBS" -lt 2 ] && DEFAULT_JOBS=2
JOBS="${E2E_JOBS:-$DEFAULT_JOBS}"
INNER="${E2E_INNER_THREADS:-2}"

echo "==> Building test binaries (cargo test --no-run)"
# `cargo test --no-run` prints an `Executable tests/<file> (<path>)` line for
# every integration-test binary even when fully cached, so parse those paths.
# (--message-format=json only emits artifacts when something recompiles.)
BUILD_OUT="$(cargo test --features e2e --no-run 2>&1)" || { echo "$BUILD_OUT"; exit 1; }
BINS=$(printf '%s\n' "$BUILD_OUT" \
       | sed -n 's/^[[:space:]]*Executable tests\/[^ ]* (\(.*\))$/\1/p' | sort -u)
N=$(printf '%s\n' "$BINS" | grep -c .)
[ "$N" -eq 0 ] && { echo "error: found no integration-test binaries"; exit 1; }
echo "==> Running $N binaries  (JOBS=$JOBS, INNER=$INNER, ~$((JOBS * INNER)) concurrent on ${CORES} cores)"

RESDIR="$(mktemp -d)"
trap 'rm -rf "$RESDIR"' EXIT
export RESDIR INNER

SECONDS=0
printf '%s\n' "$BINS" | xargs -P "$JOBS" -I{} sh -c '
  bin="$1"; name=$(basename "$bin" | sed "s/-[0-9a-f]*$//")
  if "$bin" --test-threads="$INNER" > "$RESDIR/$name.log" 2>&1; then
    echo "ok   $name"
  else
    echo "FAIL $name"
  fi
' _ {} | sort > "$RESDIR/summary.txt"
WALL=$SECONDS

PASSED=$(grep -hoE "result: ok\. [0-9]+ passed" "$RESDIR"/*.log 2>/dev/null \
         | grep -oE "[0-9]+" | paste -sd+ - | bc)
FAILS=$(grep -c '^FAIL ' "$RESDIR/summary.txt" || true)

echo "----------------------------------------------------------------"
echo "  binaries: $N    wall: ${WALL}s    tests passed: ${PASSED:-0}    failing binaries: $FAILS"
echo "----------------------------------------------------------------"
if [ "$FAILS" -gt 0 ]; then
  grep '^FAIL ' "$RESDIR/summary.txt"
  echo "--- output of failing binaries ---"
  for f in $(grep '^FAIL ' "$RESDIR/summary.txt" | awk '{print $2}'); do
    echo "===== $f ====="; tail -40 "$RESDIR/$f.log"
  done
  exit 1
fi
echo "All E2E binaries passed."
