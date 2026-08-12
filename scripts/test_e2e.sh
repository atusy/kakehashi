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
#   E2E_THREADS        test threads for the merged binaries (default: ~1.5x cores)
#   E2E_RETRIES        retry failed tests serially once     (default: 1, 0=off)
#   E2E_TIMEOUT        WHOLE-RUN timeout in seconds         (default: 600, 0=off)
#   E2E_RETRY_TIMEOUT  per-test timeout for a retry         (default: 300)
#
# NOTE: E2E_TIMEOUT caps the whole run. The per-binary runner it replaced capped
# each binary, so its effective budget was ~59x larger; a cold or heavily loaded
# machine that used to finish can now hit the cap and exit 124. Raise it rather
# than reading 124 as a hang.
#
# NOTE: the build step (`cargo test --no-run`) writes to the cargo target dir
# (`$CARGO_TARGET_DIR`, else `target/`), which rust-analyzer may lock while it
# (re)compiles after an edit — the "==> Building" line can sit for a while in
# that case. It is the editor's lock, not a hang.

# `-e` is deliberately absent: every failure below is inspected explicitly (via
# PIPESTATUS/$ec) because a failing test run is an expected outcome that must
# reach the retry and reporting logic, not abort the script.
set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

CORES=$(sysctl -n hw.ncpu 2>/dev/null || nproc 2>/dev/null || echo 4)
THREADS="${E2E_THREADS:-$(( (CORES * 3 + 1) / 2 ))}"
RETRIES="${E2E_RETRIES:-1}"
RUN_TIMEOUT="${E2E_TIMEOUT:-600}"
RETRY_TIMEOUT="${E2E_RETRY_TIMEOUT:-300}"

# Validate before use. `[ bogus -gt 0 ]` is an ERROR (status 2), not false, and
# with `-e` absent the caller would fall through to the UNCAPPED branch -- so a
# typo in E2E_TIMEOUT would silently remove the timeout it was meant to set.
# Fail loudly on anything that is not a non-negative integer.
require_uint() {
  case "$2" in
    '' | *[!0-9]*)
      echo "error: $1 must be a non-negative integer (got: '$2')" >&2
      exit 2
      ;;
  esac
}
require_uint E2E_THREADS "$THREADS"
require_uint E2E_RETRIES "$RETRIES"
require_uint E2E_TIMEOUT "$RUN_TIMEOUT"
require_uint E2E_RETRY_TIMEOUT "$RETRY_TIMEOUT"

# Honor the Makefile's `CARGO` override (toolchain selector / cargo wrapper),
# the way the other targets do — `make test_e2e` passes it through.
#
# Expanded UNQUOTED at every call site below, because the Makefile's other
# targets are `$(CARGO) test ...` and a command-style override is the normal
# way to use it: `make CARGO='cargo +stable' test` works, so
# `make CARGO='cargo +stable' test_e2e` must too. Quoting would look for one
# executable literally named "cargo +stable" and fail before running anything.
CARGO="${CARGO:-cargo}"

# A `timeout` command (GNU coreutils, or gtimeout via Homebrew) caps the run so
# one stuck test can't stall CI or a scripted gate. Optional — skipped if
# absent. (No bash array: macOS ships bash 3.2, where an empty array expansion
# trips `set -u`.)
#
# Discovered unconditionally: E2E_TIMEOUT and E2E_RETRY_TIMEOUT are documented
# as independent knobs, so gating discovery on the whole-run one would make
# E2E_TIMEOUT=0 silently disable the retry cap too.
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"

# On exit OR interrupt, kill any of THIS repo's test binaries still running
# (Ctrl-C otherwise leaves the harness's spawned servers as orphans). Scoped to
# `deps/e2e-` / `deps/integration-` under this checkout's target dir — never a
# bare `kakehashi`, which a dev may run as their editor's language server.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in /*) ;; *) TARGET_DIR="$(pwd)/$TARGET_DIR" ;; esac
# Escape every ERE metacharacter so `pkill -f` matches the path literally.
TARGET_RE="$(printf '%s' "$TARGET_DIR" | sed 's/[][(){}.^$*+?|\\]/\\&/g')"
LOG=""
# The harness kill is pattern-based, so it cannot distinguish THIS run's test
# binaries from a concurrent run's. Reserve it for the paths where cargo did not
# get to reap its own children -- Ctrl-C, SIGTERM, and the timeout -- so that a
# second `make test_e2e` (or the pre-commit hook running alongside a manual run)
# is not shot down by the first one to finish normally. On a normal exit there
# is nothing to sweep: cargo has already waited on the test binaries.
kill_orphans() {
  pkill -TERM -f "$TARGET_RE/.*/deps/(e2e|integration)-" 2>/dev/null
  if pgrep -f "$TARGET_RE/.*/deps/(e2e|integration)-" >/dev/null 2>&1; then
    sleep 1
    pkill -KILL -f "$TARGET_RE/.*/deps/(e2e|integration)-" 2>/dev/null
  fi
}
cleanup_log() {
  [ -n "$LOG" ] && rm -f "$LOG"
}
trap 'cleanup_log' EXIT
trap 'trap - EXIT; kill_orphans; cleanup_log; exit 130' INT
trap 'trap - EXIT; kill_orphans; cleanup_log; exit 143' TERM

echo "==> Building test binaries ($CARGO test --no-run)"
$CARGO test --features e2e --no-run || exit 1

# Fresh checkout: the shared parser/query install (deps/test/kakehashi) does not
# exist yet; the tests populate it lazily on first server spawn. The install
# itself is safe under concurrency (per-language flock + unique staging paths),
# but four modules own separate OnceLock initializers for that one dir
# (helpers::lsp_client, e2e_cli_diagnose, e2e_cli_format,
# e2e_config_relative_paths), so a cold parallel run has several threads
# redundantly compiling the same parsers before any of them wins. Seed it ONCE,
# serially, and the marker short-circuits every later run.
#
# The filter names a specific module, so it is a real coupling: rename or delete
# e2e_lsp_protocol and seeding silently becomes a no-op (the marker check below
# is what catches that). Seeding is best-effort — `|| true` keeps a seed failure
# from aborting a run that may well succeed anyway.
INSTALL_MARKER="deps/test/kakehashi/.installed"
if [ ! -f "$INSTALL_MARKER" ]; then
  echo "==> First run: seeding shared parser/query install (one module, serial)…"
  $CARGO test --features e2e --test e2e -- --test-threads=1 e2e_lsp_protocol:: >/dev/null 2>&1 || true
  [ -f "$INSTALL_MARKER" ] || echo "    warning: install marker still absent; the parallel run may briefly race to populate it"
fi

RUN_CAP=""
[ -n "$TIMEOUT_BIN" ] && [ "$RUN_TIMEOUT" -gt 0 ] && RUN_CAP="yes"
echo "==> Running integration + e2e  (--test-threads=$THREADS on ${CORES} cores${RUN_CAP:+, ${RUN_TIMEOUT}s cap})"
LOG="$(mktemp)"
# An empty LOG would make `tee ''` fail and leave the audit below with nothing
# to read -- fail loudly instead of running an unverifiable suite.
[ -n "$LOG" ] || { echo "error: mktemp failed; refusing to run without a log"; exit 1; }
SECONDS=0
if [ -n "$RUN_CAP" ]; then
  "$TIMEOUT_BIN" "$RUN_TIMEOUT" $CARGO test --features e2e --no-fail-fast \
    --test integration --test e2e -- --test-threads="$THREADS" 2>&1 | tee "$LOG"
else
  $CARGO test --features e2e --no-fail-fast \
    --test integration --test e2e -- --test-threads="$THREADS" 2>&1 | tee "$LOG"
fi
ec=${PIPESTATUS[0]}
WALL=$SECONDS

if [ "$ec" -eq 124 ]; then
  # timeout killed cargo mid-run, so its test binaries are orphaned here.
  kill_orphans
  echo "error: run exceeded the ${RUN_TIMEOUT}s timeout (E2E_TIMEOUT to raise)"
  exit 124
fi

# Everything below trusts the log to say what ran and what failed, so check the
# log is COMPLETE before trusting it. Cargo's one-target-per-file layout used to
# make this free — a missing binary was a missing binary — and the old runner
# cross-checked the discovered binaries and refused a partial run. Merging to two
# targets removed both safety nets, leaving three ways for a red run to read
# green:
#   * a binary dies by signal (abort, SIGSEGV, stack overflow, OOM-kill) and
#     never prints a `test result:` line, so its failures are invisible to the
#     retry parser below and the surviving binary's flake "recovers" the run;
#   * a binary runs zero tests — e.g. `--features e2e` stops enabling the module
#     tree, making tests/e2e a `#![cfg]`-emptied harness — and libtest exits 0;
#   * libtest declares more failures than the parser can name, so the unnamed
#     ones are never retried and silently disappear.
# All three are fatal: refuse the run rather than fall through to a retry.
# Default to 0 rather than empty: `[ "" -ne 2 ]` is an error, and with `-e`
# absent the `if` reads that error as FALSE and skips the very check below —
# an unreadable log would silently disable the audit instead of tripping it.
SUMMARIES=$(grep -c '^test result: ' "$LOG" 2>/dev/null)
SUMMARIES=${SUMMARIES:-0}
if [ "$SUMMARIES" -ne 2 ]; then
  echo "error: expected a 'test result:' summary from both binaries, found $SUMMARIES."
  echo "       A test binary died without reporting (abort/segfault/stack overflow/OOM)."
  exit 1
fi
if grep -q '^running 0 tests' "$LOG"; then
  echo "error: a test binary ran zero tests — the suite is not being exercised."
  echo "       Check that --features e2e still enables the tests/e2e module tree."
  exit 1
fi
# "more than zero tests" is not the same as "the suite ran". tests/e2e/helpers
# contributes 25 #[test] fns of its own, so an e2e binary containing nothing but
# helpers still reports a healthy-looking count. Require each suite's actual
# test modules to have produced results.
for want in 'e2e_:E2E' 'test_:integration'; do
  # Anchored on libtest's complete result-line shape, and on a status that means
  # the body actually EXECUTED. Two reasons: a failing test's captured stdout is
  # echoed into this same log, so a loose match could be satisfied by a test
  # merely PRINTING such a line; and `... ignored` would otherwise let a wholly
  # #[ignore]d suite satisfy the floor without running anything.
  if ! grep -qE "^test ${want%%:*}[A-Za-z0-9_:]+ \.\.\. (ok|FAILED)$" "$LOG"; then
    echo "error: no ${want%%:*}* test ran — the ${want##*:} suite was not exercised."
    echo "       Its modules are missing, unregistered, or filtered out."
    exit 1
  fi
done

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

# Every failure libtest declared must be one the parser could name. If the counts
# disagree, some failure is about to go unretried and therefore unreported — the
# parser's charset missed a name, or a `failures:` block was truncated. Refuse,
# rather than retry the subset and call the run green.
DECLARED=$(awk '/^test result: /{for(i=1;i<=NF;i++) if($(i+1)=="failed;") s+=$i} END{print s+0}' "$LOG")
PARSED=$(printf '%s\n' "$FAILED" | grep -c .)
DECLARED=${DECLARED:-0}
PARSED=${PARSED:-0}
if [ "$DECLARED" -ne "$PARSED" ]; then
  echo "error: libtest declared $DECLARED failure(s) but only $PARSED could be named for retry."
  echo "       Refusing to retry a subset — the unnamed failure(s) would vanish."
  exit "$ec"
fi

# Bound each retry the way the main run is bounded. Without this a test that
# hangs only when run alone blocks the gate (and the pre-commit hook) forever:
# the whole-run cap has already been spent by the time we get here.
run_retry() {
  if [ -n "$TIMEOUT_BIN" ] && [ "$RETRY_TIMEOUT" -gt 0 ]; then
    "$TIMEOUT_BIN" "$RETRY_TIMEOUT" $CARGO test --features e2e --no-fail-fast \
      --test integration --test e2e -- --exact "$1" --test-threads=1 2>&1
  else
    $CARGO test --features e2e --no-fail-fast \
      --test integration --test e2e -- --exact "$1" --test-threads=1 2>&1
  fi
}

echo
echo "==> Retrying $(printf '%s\n' "$FAILED" | wc -l | tr -d ' ') failed tests serially (load-induced flakes recover here):"
final=0
# Unquoted on purpose: the parser's charset admits no whitespace or glob
# characters, so word-splitting $FAILED is exactly the intended per-name split.
for t in $FAILED; do
  t0=$SECONDS
  out=$(run_retry "$t")
  rc=$?
  # A filter that matches nothing makes libtest exit 0, so the exit code alone
  # cannot tell "passed" from "never ran". Count what actually executed.
  ran=$(printf '%s\n' "$out" | awk '/^test result: /{for(i=1;i<=NF;i++){if($(i+1)=="passed;")p+=$i; if($(i+1)=="failed;")f+=$i}} END{print p+f+0}')
  if [ "$rc" -eq 0 ] && [ "$ran" -ge 1 ]; then
    printf '  retry ok   %s  %3ds (was a flake)\n' "$t" "$((SECONDS - t0))"
  elif [ "$rc" -eq 0 ]; then
    printf '  retry VOID %s  %3ds (matched no test — name unusable, not a pass)\n' "$t" "$((SECONDS - t0))"
    final=1
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
