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
  local dir="$1" main="$2" stem missing
  missing=""
  for f in "$dir"/*.rs; do
    stem="$(basename "$f" .rs)"
    [ "$stem" = "main" ] && continue
    grep -q "^mod ${stem};" "$main" || missing="${missing}  ${f}"$'\n'
  done
  if [ -n "$missing" ]; then
    echo "error: test file(s) not declared in ${main} — they are silently never run:" >&2
    printf '%s' "$missing" >&2
    echo "       add the matching \`mod <name>;\` line." >&2
    status=1
  fi
}

check_dir tests/e2e tests/e2e/main.rs
check_dir tests/integration tests/integration/main.rs

# The reverse direction: a `mod` line whose file was deleted or renamed is a
# hard compile error, so cargo already covers it — no check needed here.

exit "$status"
