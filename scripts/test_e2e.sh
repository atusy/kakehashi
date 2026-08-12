#!/usr/bin/env bash
#
# Run the integration + E2E suites with tuned parallelism and per-test flake
# retry.
#
# The suites live in TWO merged binaries (tests/integration/, tests/e2e/) so a
# lib edit links twice, not ~59 times (see tests/e2e/main.rs). libtest already
# runs the tests inside a binary on parallel threads; each E2E test spends most
# of its wall time waiting on a spawned `kakehashi` server (and, for bridge
# tests, on `lua-language-server`), so the default thread count (= cores)
# leaves the machine idle. ~1.5x cores was the measured sweet spot when the
# old per-binary pool runner tuned total concurrency; oversubscribing further
# makes timing-sensitive tests and the lua-ls poll loops thrash.
#
# A test that fails under that load is retried serially (E2E_RETRIES=1 by
# default): a spawned server can miss its internal 10-30s startup timeout when
# the machine is busy (e.g. a rust-analyzer compile storm) and recovers given
# the machine to itself. A genuinely broken test fails both times.
#
# Usage:
#   scripts/test_e2e.sh
#   E2E_THREADS=8 E2E_TIMEOUT=900 E2E_RETRIES=0 scripts/test_e2e.sh
#
# Env:
#   E2E_THREADS   test threads for the merged binaries (default: ~1.5x cores)
#   E2E_RETRIES   serial retry rounds for failed tests   (default: 1, 0=off)
#   E2E_TIMEOUT   whole-run timeout in seconds           (default: 600, 0=off)
#
# NOTE: the build step (`cargo test --no-run`) writes to the default `target/`,
# which rust-analyzer may lock while it (re)compiles after an edit — the
# "==> Building" line can sit for a while in that case. It is the editor's
# lock, not a hang.

set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

CORES=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)
THREADS="${E2E_THREADS:-$(( (CORES * 3 + 1) / 2 ))}"
RETRIES="${E2E_RETRIES:-1}"
RUN_TIMEOUT="${E2E_TIMEOUT:-600}"

# Honor the Makefile's `CARGO` override (toolchain selector / cargo wrapper),
# the way the other targets do — `make test_e2e` passes it through.
CARGO="${CARGO:-cargo}"

# A `timeout` command (GNU coreutils, or gtimeout via Homebrew) caps the run so
# one stuck test can't stall CI or a scripted gate. Optional — skipped if
# absent or disabled. (No bash array: macOS ships bash 3.2, where an empty
# array expansion trips `set -u`.)
TIMEOUT_BIN=""
if [ "$RUN_TIMEOUT" -gt 0 ]; then
  TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
fi

# On exit OR interrupt, kill any of THIS repo's test binaries still running
# (Ctrl-C otherwise leaves the harness's spawned servers as orphans). Scoped to
# `deps/e2e-` / `deps/integration-` under this checkout's target dir — never a
# bare `kakehashi`, which a dev may run as their editor's language server.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$(pwd)/$TARGET_DIR" ;; esac
# Escape every ERE metacharacter so `pkill -f` matches the path literally.
TARGET_RE="$(printf '%s' "$TARGET_DIR" | sed 's/[][(){}.^$*+?|\\]/\\&/g')"
LOG=""
cleanup() {
  pkill -TERM -f "$TARGET_RE/.*/deps/(e2e|integration)-" 2>/dev/null
  if pgrep -f "$TARGET_RE/.*/deps/(e2e|integration)-" >/dev/null 2>&1; then
    sleep 1
    pkill -KILL -f "$TARGET_RE/.*/deps/(e2e|integration)-" 2>/dev/null
  fi
  [ -n "$LOG" ] && rm -f "$LOG"
}
trap 'cleanup' EXIT
trap 'trap - EXIT; cleanup; exit 130' INT
trap 'trap - EXIT; cleanup; exit 143' TERM

echo "==> Building test binaries ($CARGO test --no-run)"
"$CARGO" test --features e2e --no-run || exit 1

# Fresh checkout: the shared parser/query install (deps/test/kakehashi) does
# not exist yet. The tests populate it lazily on first server spawn, so a
# parallel run would have many tests race to populate one dir — one reading
# another's half-written parser/query files (corrupt-cache flakiness). Seed it
# ONCE, serially, with a single module first; the marker then short-circuits
# every later run.
INSTALL_MARKER="deps/test/kakehashi/.installed"
if [ ! -f "$INSTALL_MARKER" ]; then
  echo "==> First run: seeding shared parser/query install (one module, serial)…"
  "$CARGO" test --features e2e --test e2e -- --test-threads=1 e2e_lsp_protocol:: >/dev/null 2>&1 || true
  [ -f "$INSTALL_MARKER" ] || echo "    warning: install marker still absent; the parallel run may briefly race to populate it"
fi

echo "==> Running integration + e2e  (--test-threads=$THREADS on ${CORES} cores${TIMEOUT_BIN:+, ${RUN_TIMEOUT}s cap})"
LOG="$(mktemp)"
SECONDS=0
if [ -n "$TIMEOUT_BIN" ]; then
  "$TIMEOUT_BIN" "$RUN_TIMEOUT" "$CARGO" test --features e2e --no-fail-fast \
    --test integration --test e2e -- --test-threads="$THREADS" 2>&1 | tee "$LOG"
else
  "$CARGO" test --features e2e --no-fail-fast \
    --test integration --test e2e -- --test-threads="$THREADS" 2>&1 | tee "$LOG"
fi
ec=${PIPESTATUS[0]}
WALL=$SECONDS

if [ "$ec" -eq 124 ]; then
  echo "error: run exceeded the ${RUN_TIMEOUT}s timeout (E2E_TIMEOUT to raise)"
  exit 124
fi
if [ "$ec" -eq 0 ]; then
  echo "All tests passed (${WALL}s)."
  exit 0
fi
[ "$RETRIES" -gt 0 ] || exit "$ec"

# Collect failed test names from libtest's `failures:` sections (indented
# `module::test` lines up to the following blank line; the detailed and
# summary sections repeat the same names — the sort dedupes).
FAILED=$(awk '/^failures:$/{f=1;next} f&&/^$/{f=0} f&&/^    [a-z_][a-zA-Z0-9_:]*$/{sub(/^    /,"");print}' "$LOG" | sort -u)
if [ -z "$FAILED" ]; then
  echo "error: run failed but no test names could be parsed for retry"
  exit "$ec"
fi

echo
echo "==> Retrying $(printf '%s\n' "$FAILED" | wc -l | tr -d ' ') failed tests serially (load-induced flakes recover here):"
final=0
for t in $FAILED; do
  t0=$SECONDS
  if "$CARGO" test --features e2e --no-fail-fast --test integration --test e2e \
       -- --exact "$t" --test-threads=1 >/dev/null 2>&1; then
    printf '  retry ok   %s  %3ds (was a flake)\n' "$t" "$((SECONDS - t0))"
  else
    printf '  retry FAIL %s  %3ds (real failure)\n' "$t" "$((SECONDS - t0))"
    final=1
  fi
done
echo
if [ "$final" -eq 0 ]; then
  echo "All tests passed after retry (${SECONDS}s total)."
else
  echo "Real failures remain after retry (${SECONDS}s total)."
fi
exit "$final"
