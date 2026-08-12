#!/usr/bin/env bash
#
# Assert every test file is declared in its merged binary's module list.
#
# The integration and E2E suites used to be one cargo target per file, so cargo
# discovered them: dropping a file into tests/ was enough to make it run. They
# are now two merged binaries whose contents come from hand-written `mod` lists
# (tests/e2e/main.rs, tests/integration/main.rs). A file with no `mod` line is
# not a compile error — it is simply never compiled and never run, and the suite
# stays green while the coverage is gone.
#
# The per-binary runner this replaced refused to run when the binary count did
# not match the file count. This is that guard. It is wired into BOTH `make
# check` (local + pre-commit hook) and the CI `check` job directly, because CI
# invokes the cargo commands itself rather than going through `make check` —
# wiring it only into the Makefile would leave CI unguarded.

set -uo pipefail

cd "$(git rev-parse --show-toplevel)"

status=0

# $1: directory holding the modules, $2: the main.rs declaring them
check_dir() {
  local dir="$1" main="$2" stem missing found
  missing=""
  found=0
  for f in "$dir"/*.rs; do
    stem="$(basename "$f" .rs)"
    [ "$stem" = "main" ] && continue
    found=$((found + 1))
    grep -q "^mod ${stem};" "$main" || missing="${missing}  ${f}"$'\n'
  done
  # Directory-backed modules (<name>/mod.rs) too, or they would be invisible in
  # this direction while the reverse check below happily accepts them.
  for f in "$dir"/*/mod.rs; do
    [ -f "$f" ] || continue
    stem="$(basename "$(dirname "$f")")"
    found=$((found + 1))
    grep -q "^mod ${stem};" "$main" || missing="${missing}  ${f}"$'\n'
  done
  # A suite of nothing but main.rs would satisfy every check above vacuously,
  # and CI's plain `cargo test` would happily accept the resulting zero-test
  # binary. A guard that passes when there is nothing to guard is worse than
  # no guard, so require the suite to be non-empty.
  # (A missing directory needs no separate case: bash leaves the unmatched glob
  # literal, so the "*" stem finds no `mod` line and is reported as missing.)
  if [ "$found" -eq 0 ]; then
    echo "error: ${dir} declares no test modules at all — the suite would be empty." >&2
    status=1
    return
  fi
  if [ -n "$missing" ]; then
    echo "error: test file(s) not declared in ${main} — they are silently never run:" >&2
    printf '%s' "$missing" >&2
    echo "       add the matching \`mod <name>;\` line." >&2
    status=1
  fi

  # And the reverse: a `mod` line whose file is gone. This is NOT covered by
  # cargo the way it would be in ordinary code. `cargo check` and `cargo clippy`
  # (as `make check` and CI run them) do not build test targets at all, and CI's
  # `cargo test` builds tests/e2e without `--features e2e`, so the crate-level
  # `#![cfg(feature = "e2e")]` strips the whole module tree before any `mod` is
  # resolved. Deleting a test file and leaving its `mod` line would therefore
  # sail through CI, silently dropping that file's coverage.
  local name
  for name in $(sed -n 's/^mod \([a-z_][a-z0-9_]*\);.*/\1/p' "$main"); do
    # `mod helpers;` resolves to helpers/mod.rs, so accept either spelling.
    if [ ! -f "${dir}/${name}.rs" ] && [ ! -f "${dir}/${name}/mod.rs" ]; then
      echo "error: ${main} declares \`mod ${name};\` but neither ${dir}/${name}.rs nor ${dir}/${name}/mod.rs exists." >&2
      status=1
    fi
  done
}

check_dir tests/e2e tests/e2e/main.rs
check_dir tests/integration tests/integration/main.rs

exit "$status"
