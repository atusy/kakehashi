#!/usr/bin/env bash
#
# Run the E2E / integration test binaries CONCURRENTLY, with live progress.
#
# `cargo test` runs integration-test binaries one at a time (it only parallelizes
# the test functions *within* a single binary). Each of our E2E binaries spends
# most of its wall time waiting on a spawned `kakehashi` server (and, for the
# bridge tests, on `lua-language-server`), so a sequential run leaves a many-core
# machine almost idle — the full suite takes ~95s here at ~1.2x core utilization.
#
# This script builds the binaries once, then runs them in a bounded parallel
# pool, printing each binary's result as it finishes. The bound matters:
# oversubscribing (e.g. 30 concurrent test threads on 10 cores) makes
# timing-sensitive tests and the lua-ls poll loops thrash and run SLOWER than
# sequential. Total concurrency ~1.5x cores is the sweet spot (~18s, ~5x).
#
# Safe to run concurrently because each spawned server is isolated:
#   - KAKEHASHI_DATA_DIR  : shared, read-only after install (parsers/queries)
#   - XDG_CONFIG_HOME     : per-process temp dir (no user-config bleed)
#   - KAKEHASHI_STATE_DIR : per-SPAWN temp dir (crash-recovery state, set by the
#                           test harness so no two concurrent servers — threads
#                           or processes — ever share/poison those files)
#
# Usage:
#   scripts/test_e2e_parallel.sh
#   E2E_JOBS=8 E2E_INNER_THREADS=2 E2E_BIN_TIMEOUT=180 scripts/test_e2e_parallel.sh
#
# Env:
#   E2E_JOBS           number of test BINARIES to run at once (default: ~0.75x cores)
#   E2E_INNER_THREADS  test threads WITHIN each binary       (default: 2)
#                      → total concurrency ≈ E2E_JOBS * E2E_INNER_THREADS
#   E2E_BIN_TIMEOUT    per-binary timeout in seconds         (default: 600, 0=off)
#   E2E_RETRIES        serial retries for a FAILED binary    (default: 1, 0=off)
#                      → recovers load-induced timeout flakes; real failures persist
#
# NOTE: the build step (`cargo test --no-run`) writes to the default `target/`,
# which rust-analyzer may lock while it (re)compiles after an edit — the
# "==> Building" line can sit for a while in that case. It is the editor's lock,
# not a hang. Under heavy CPU load (editor + rust-analyzer compiling) the run
# also slows and per-binary times climb; the timeout is deliberately generous so
# a busy machine reports "slow", never a spurious failure. The pass count is
# printed at the end — eyeball it against your sequential baseline; a much lower
# count means binaries silently skipped, not that the run was clean.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

CORES=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)
DEFAULT_JOBS=$(( (CORES * 3 + 3) / 4 ))
[ "$DEFAULT_JOBS" -lt 2 ] && DEFAULT_JOBS=2
JOBS="${E2E_JOBS:-$DEFAULT_JOBS}"
INNER="${E2E_INNER_THREADS:-2}"
BIN_TIMEOUT="${E2E_BIN_TIMEOUT:-600}"

# A `timeout` command (GNU coreutils, or gtimeout via Homebrew) lets us cap each
# binary so one stuck test can't stall the whole pool. Optional — skipped if
# absent or disabled.
TIMEOUT_BIN=""
if [ "$BIN_TIMEOUT" -gt 0 ]; then
  TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
fi

# Honor the Makefile's `CARGO` override (toolchain selector / cargo wrapper),
# the way the other targets do — `make test_e2e` passes it through.
CARGO="${CARGO:-cargo}"

echo "==> Building test binaries ($CARGO test --no-run)"
# `cargo test --no-run` prints an `Executable tests/<file> (<path>)` line for
# every integration-test binary even when fully cached, so parse those paths.
# (--message-format=json only emits artifacts when something recompiles.)
BUILD_OUT="$("$CARGO" test --features e2e --no-run 2>&1)" || { echo "$BUILD_OUT"; exit 1; }
BINS=$(printf '%s\n' "$BUILD_OUT" \
       | sed -n 's/^[[:space:]]*Executable tests\/[^ ]* (\(.*\))$/\1/p' | sort -u)
N=$(printf '%s\n' "$BINS" | grep -c .)
[ "$N" -eq 0 ] && { echo "error: found no integration-test binaries"; exit 1; }
# Independent expected count: every top-level tests/*.rs is one integration
# binary (helpers/ is a subdir module, not a binary). If the Executable-line
# parse ever silently under-counts (a cargo output format change), N drops below
# this and we fail loudly instead of running a smaller-but-green subset.
EXPECTED=$(ls tests/*.rs 2>/dev/null | wc -l | tr -d ' ')
if [ "$N" -ne "$EXPECTED" ]; then
  echo "error: parsed $N test binaries but tests/ has $EXPECTED .rs files — the"
  echo "       Executable-line parse may be under-counting; refusing a partial run."
  exit 1
fi

RESDIR="$(mktemp -d)"
# On exit OR interrupt: drop the temp dir and kill any of THIS repo's test
# binaries still running (Ctrl-C otherwise leaves them as a pool of orphans).
# We deliberately kill ONLY the `deps/e2e_*` / `deps/test_*` binaries, never a
# bare `kakehashi` — a dev may run THIS repo's `target/debug/kakehashi` as their
# editor's language server (see __ignored/kakehashi.bash), and a path-based
# pkill cannot tell that apart from a test-spawned server. The test servers
# instead get stdin EOF when their parent test binary dies and exit on their
# own; their lua-language-server grandchildren may briefly orphan to PID 1 on a
# hard Ctrl-C (pkill them by name manually if needed — but that also hits an
# editor's lua-ls, so it's left to the user).
# Where the test binaries actually live — honor CARGO_TARGET_DIR (the README/PR
# even suggests a separate one to dodge rust-analyzer's lock), resolving a
# relative value against the repo root, so the cleanup below still finds them.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$(pwd)/$TARGET_DIR" ;; esac
# Escape every ERE metacharacter so `pkill -f` matches the path literally.
# (Typical paths only hit '.' from "github.com", but a checkout under a path with
# +, ?, (), {}, |, etc. would otherwise broaden the pattern.) The `deps/e2e_`/
# `deps/test_` anchor keeps the kill scoped to THIS run's test binaries — never a
# bare `kakehashi` a dev might run as their editor LSP. The char class lists ']'
# first so it's literal.
TARGET_RE="$(printf '%s' "$TARGET_DIR" | sed 's/[][(){}.^$*+?|\\]/\\&/g')"
cleanup() {
  pkill -f "$TARGET_RE/.*/deps/e2e_" 2>/dev/null
  pkill -f "$TARGET_RE/.*/deps/test_" 2>/dev/null
  rm -rf "$RESDIR"
}
trap cleanup EXIT INT TERM

# Fresh checkout: the shared parser/query install (deps/test/kakehashi) does not
# exist yet. The test binaries create it lazily on first server spawn, so the
# parallel pool below would have many processes race to populate one dir — one
# reading another's half-written parser/query files (corrupt-cache flakiness, an
# early `.installed`). Seed it ONCE, serially, with a single binary first; the
# marker then short-circuits every later run (and the rest of this run's pool).
# Bounded to one binary on purpose: grinding through all of them silently would
# reintroduce the "looks hung" problem this runner exists to avoid.
INSTALL_MARKER="deps/test/kakehashi/.installed"
if [ ! -f "$INSTALL_MARKER" ]; then
  echo "==> First run: seeding shared parser/query install (serial, one binary)…"
  seed="$(printf '%s\n' "$BINS" | head -1)"
  if [ -n "$seed" ]; then
    "$seed" --test-threads=1 >/dev/null 2>&1 || true
  fi
  [ -f "$INSTALL_MARKER" ] || echo "    warning: install marker still absent; the pool may briefly race to populate it"
fi

echo "==> Running $N binaries  (JOBS=$JOBS, INNER=$INNER, ~$((JOBS * INNER)) concurrent on ${CORES} cores${TIMEOUT_BIN:+, ${BIN_TIMEOUT}s/binary})"
echo "    (progress streams below as each binary finishes)"
echo

export RESDIR INNER TIMEOUT_BIN BIN_TIMEOUT N
SECONDS=0
# Each job runs one binary, then prints a one-line progress record immediately
# (so the user sees the suite advancing) and records its status for the summary.
printf '%s\n' "$BINS" | xargs -P "$JOBS" -I{} sh -c '
  bin="$1"; name=$(basename "$bin" | sed "s/-[0-9a-f]*$//")
  t0=$(date +%s)
  if [ -n "$TIMEOUT_BIN" ]; then
    "$TIMEOUT_BIN" "$BIN_TIMEOUT" "$bin" --test-threads="$INNER" > "$RESDIR/$name.log" 2>&1
  else
    "$bin" --test-threads="$INNER" > "$RESDIR/$name.log" 2>&1
  fi
  ec=$?
  dt=$(( $(date +%s) - t0 ))
  case $ec in
    0)   status="ok  " ;;
    124) status="TIME" ;;   # killed by timeout
    *)   status="FAIL" ;;
  esac
  echo "$ec" > "$RESDIR/$name.status"
  done=$(ls "$RESDIR"/*.status 2>/dev/null | wc -l | tr -d " ")
  printf "  [%2d/%2d] %s %-34s %3ds\n" "$done" "$N" "$status" "$name" "$dt"
' _ {}
WALL=$SECONDS

# Retry any failed binaries ONCE, serially. Under heavy machine load (e.g. a
# rust-analyzer compile storm) a spawned server or mock bridge can miss its
# internal startup timeout (10–30s) and the whole binary fails spuriously; given
# the machine to itself on retry it succeeds. A genuinely broken test still
# fails both times. Set E2E_RETRIES=0 to disable.
RETRY="${E2E_RETRIES:-1}"
if [ "$RETRY" -gt 0 ]; then
  retry_names=""
  for s in "$RESDIR"/*.status; do
    [ -f "$s" ] || continue
    [ "$(cat "$s")" != 0 ] && retry_names="$retry_names $(basename "$s" .status)"
  done
  if [ -n "${retry_names# }" ]; then
    echo
    echo "==> Retrying failed binaries serially ($(printf '%s' "$retry_names" | wc -w | tr -d ' ') — load-induced flakes recover here):"
    for name in $retry_names; do
      bin="$(printf '%s\n' "$BINS" | grep "/deps/${name}-" | head -1)"
      [ -n "$bin" ] || continue
      t0=$(date +%s)
      if [ -n "$TIMEOUT_BIN" ]; then
        "$TIMEOUT_BIN" "$BIN_TIMEOUT" "$bin" --test-threads=1 > "$RESDIR/$name.log" 2>&1
      else
        "$bin" --test-threads=1 > "$RESDIR/$name.log" 2>&1
      fi
      ec=$?; echo "$ec" > "$RESDIR/$name.status"
      dt=$(( $(date +%s) - t0 ))
      if [ "$ec" = 0 ]; then printf "  retry ok   %-34s %3ds (was a flake)\n" "$name" "$dt"
      else                   printf "  retry FAIL %-34s %3ds (real failure)\n" "$name" "$dt"; fi
    done
  fi
fi

# Sum with awk (no `bc` dependency — not installed on minimal environments).
PASSED=$(grep -hoE "result: ok\. [0-9]+ passed" "$RESDIR"/*.log 2>/dev/null \
         | grep -oE "[0-9]+" | awk '{s += $1} END {print s + 0}')
FAILS=$(grep -lvx 0 "$RESDIR"/*.status 2>/dev/null | wc -l | tr -d ' ')
RAN=$(ls "$RESDIR"/*.status 2>/dev/null | wc -l | tr -d ' ')

echo
echo "----------------------------------------------------------------"
echo "  binaries: $N    wall: ${WALL}s    tests passed: ${PASSED:-0}    failing binaries: $FAILS"
echo "----------------------------------------------------------------"
# Guard against a binary being silently dropped (e.g. a future cargo output
# change defeating the Executable-line parse): every discovered binary must have
# produced a status file, else coverage shrank without any binary "failing".
if [ "$RAN" -ne "$N" ]; then
  echo "error: only $RAN/$N binaries ran — some were dropped before execution"
  exit 1
fi
if [ "$FAILS" -gt 0 ]; then
  echo "--- output of failing/timed-out binaries ---"
  for s in $(grep -lvx 0 "$RESDIR"/*.status 2>/dev/null); do
    f="${s%.status}"; echo "===== $(basename "$f") (exit $(cat "$s")) ====="; tail -40 "$f.log"
  done
  exit 1
fi
echo "All E2E binaries passed."
