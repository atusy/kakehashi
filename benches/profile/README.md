# Semantic-tokens profiling harness

Flamegraph profiling of the `semanticTokens/full` hot path, used to find and
verify bottlenecks that the A/B benchmark (`benches/semantic_tokens.rs`) then
quantifies.

> **macOS only.** Offline symbolication relies on `.dSYM` + `dsymutil`/`atos`,
> and `analyze.py` assumes the macOS `__TEXT` base. `profile.sh` checks for this
> and fails early elsewhere. (The benchmark in `benches/semantic_tokens.rs` is
> cross-platform; only this profiling harness is macOS-specific.)

## Quick start

```sh
cargo install samply inferno          # one-time
cargo test --features e2e             # one-time: populate deps/test/kakehashi with parsers
benches/profile/profile.sh --lang rust --size 150 --requests 150
# -> $TMPDIR/kakehashi-profile/flamegraph.svg  (+ a top-functions report on stdout)
```

For bridge-level latency and output-volume measurements on a real document,
build a release binary and use the synchronous driver directly:

```sh
cargo build --release --bin kakehashi
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --file path/to/input.md --requests 20 --edits 1

# Queue a captures delta first, then semantic tokens, to reproduce shared-pool and
# response-output contention from an already-busy highlighter client.
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --file path/to/input.md --requests 20 --edits 1 --concurrent-captures

# Send superseding requests in bursts to measure cancellation pressure.
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --file path/to/input.md --requests 20 --burst 8 --burst-edits
```

The driver reports per-method p50/p90/max request-to-response latency both
overall and split by outcome (`ok`, cancelled, `null`, error), time to the last
successful semantic response in each cycle, exact JSON response-body bytes,
and server notifications/requests grouped separately. This keeps completed
semantic compute latency separate from cheap supersession responses and from
large captures or diagnostic output that may dominate a cycle.

With full Xcode installed, Instruments' Time Profiler can also be used from the
CLI:

```sh
benches/profile/xctrace.sh --lang markdown --size 150 --requests 160 --edits 1
# -> $TMPDIR/kakehashi-xctrace/semantic-time.trace (+ a target-only summary)
```

Allocation and retained-heap profilers need to attach to the spawned server
rather than the Python driver. The driver exposes an optional file handshake so
an external profiler can attach to the exact PID after an awaited, unmeasured
semantic-token warmup and before measured requests begin:

```sh
cargo build --profile profiling --bin kakehashi || exit $?
profile_dir="$(mktemp -d "${TMPDIR:-/tmp}/kakehashi-profile.XXXXXX")" || exit $?
profile_wait_seconds=300
profile_hold_seconds=60
controller_grace_seconds=12
driver_pid=

driver_job_is_running() {
  [ -n "$driver_pid" ] || return 1
  jobs_file="$profile_dir/controller-jobs"
  jobs -l > "$jobs_file" 2>/dev/null || return 1
  awk -v pid="$driver_pid" '
    {
      state = tolower($0)
      for (field = 1; field <= NF; field++)
        if ($field == pid && state !~ /(done|exit|terminated)/) found = 1
    }
    END { exit !found }
  ' "$jobs_file"
  jobs_status=$?
  rm -f "$jobs_file"
  return "$jobs_status"
}

wait_for_driver_exit() {
  timeout_seconds="$1"
  wait_pid="$driver_pid"
  [ -n "$wait_pid" ] || return 0
  deadline=$(( $(date +%s) + timeout_seconds ))
  while driver_job_is_running; do
    if [ "$(date +%s)" -ge "$deadline" ]; then
      if driver_job_is_running; then
        kill -KILL "$wait_pid" 2>/dev/null || true
      fi
      wait "$wait_pid" 2>/dev/null || true
      driver_pid=
      return 124
    fi
    sleep 0.05
  done
  wait "$wait_pid"
  wait_status=$?
  driver_pid=
  return "$wait_status"
}

cleanup_profile_run() {
  trap - EXIT HUP INT TERM
  if driver_job_is_running; then
    kill "$driver_pid" 2>/dev/null || true
  fi
  wait_for_driver_exit "$controller_grace_seconds" || true
  [ ! -d "$profile_dir" ] || rm -r "$profile_dir"
}
trap cleanup_profile_run EXIT
trap 'trap - EXIT HUP INT TERM; cleanup_profile_run; exit 129' HUP
trap 'trap - EXIT HUP INT TERM; cleanup_profile_run; exit 130' INT
trap 'trap - EXIT HUP INT TERM; cleanup_profile_run; exit 143' TERM

python3 benches/profile/drive.py \
  --bin ./target/profiling/kakehashi \
  --file path/to/input.md --requests 80 --edits 1 \
  --profile-pid-file "$profile_dir/pid" \
  --profile-ready-file "$profile_dir/ready" \
  --profile-start-file "$profile_dir/start" \
  --profile-done-file "$profile_dir/done" \
  --profile-stop-file "$profile_dir/stop" \
  --profile-wait-timeout "$profile_wait_seconds" \
  --profile-hold-seconds "$profile_hold_seconds" &
driver_pid=$!

wait_for_driver_marker() {
  marker="$1"
  timeout_seconds="$2"
  deadline=$(( $(date +%s) + timeout_seconds ))
  while [ ! -f "$marker" ]; do
    if ! driver_job_is_running; then
      wait_for_driver_exit "$controller_grace_seconds"
      driver_status=$?
      [ -f "$marker" ] && return 0
      [ "$driver_status" -ne 0 ] || driver_status=1
      printf 'driver exited before publishing %s\n' "$marker" >&2
      return "$driver_status"
    fi
    if [ "$(date +%s)" -ge "$deadline" ]; then
      printf 'controller timed out waiting for %s\n' "$marker" >&2
      return 124
    fi
    sleep 0.05
  done
}

# Wait until the server PID is complete and the semantic warmup has responded.
wait_for_driver_marker "$profile_dir/ready" "$profile_wait_seconds" || exit $?
[ -s "$profile_dir/pid" ] || {
  printf 'driver published ready without a PID\n' >&2
  exit 1
}
server_pid="$(cat "$profile_dir/pid")"
server_identity="$(
  ps -o ppid= -o lstart= -p "$server_pid" |
    awk '{$1 = $1; print}'
)"
[ "${server_identity%% *}" = "$driver_pid" ] || {
  printf 'could not record profile target identity\n' >&2
  exit 1
}

profile_target_is_live() {
  driver_job_is_running &&
    [ -s "$profile_dir/pid" ] &&
    [ "$(cat "$profile_dir/pid")" = "$server_pid" ] &&
    kill -0 "$server_pid" 2>/dev/null &&
    [ "$(
      ps -o ppid= -o lstart= -p "$server_pid" |
        awk '{$1 = $1; print}'
    )" = "$server_identity" ]
}

require_profile_target() {
  profile_target_is_live && return 0
  printf 'profile target exited or invalidated PID marker\n' >&2
  return 1
}

require_profile_target || exit $?
printf 'Attach the profiler to kakehashi PID %s\n' "$server_pid"
```

Pause here and attach the profiler to `"$server_pid"`. After it confirms the
attachment, run the second block in the same shell to release only the measured
semantic-token workload:

```sh
require_profile_target || exit $?
touch "$profile_dir/start" || exit $?

# "$profile_dir/done" marks the end of measured requests. The server remains
# alive during the hold interval so retained-heap tools can inspect it.
wait_for_driver_marker "$profile_dir/done" "$profile_wait_seconds" || exit $?
require_profile_target || exit $?
python3 -c '
import subprocess
import sys

try:
    completed = subprocess.run(sys.argv[2:], timeout=float(sys.argv[1]))
except subprocess.TimeoutExpired:
    sys.exit(124)
sys.exit(completed.returncode)
' "$profile_hold_seconds" /usr/bin/heap -H "$server_pid"
heap_status=$?
touch "$profile_dir/stop"
stop_status=$?
wait_for_driver_exit "$controller_grace_seconds"
driver_status=$?
rm -r "$profile_dir"
remove_status=$?
[ "$remove_status" -ne 0 ] || trap - EXIT HUP INT TERM
[ "$heap_status" -eq 0 ] || exit "$heap_status"
[ "$stop_status" -eq 0 ] || exit "$stop_status"
[ "$driver_status" -eq 0 ] || exit "$driver_status"
[ "$remove_status" -eq 0 ] || exit "$remove_status"
```

Use a fresh marker directory for each run. `pid`, `ready`, and `done` are
published atomically. `start` and `stop` are owned by the profiler controller.
If `start` is not published within `--profile-wait-timeout`, the driver fails
and still reaps the server. The interactive example allows five minutes for
attachment; the CLI default is 30 seconds. The same timeout bounds the semantic
warmup response, while shutdown responses are capped at five seconds. After
`done`, `stop` releases the server as soon as the retained-heap snapshot
finishes; `--profile-hold-seconds` is a 60-second safety deadline if the
controller never publishes it. Independently, the shell controller caps
`ready`/`done` polling at `profile_wait_seconds`, the foreground heap command
at `profile_hold_seconds`, and driver cleanup at `controller_grace_seconds`
(12 seconds, longer than the driver's bounded shutdown phases). These variables
can be raised for intentionally longer profiling runs.

The harness drives the server against `deps/test/kakehashi` for parsers/queries.
If that dir has no installed parsers, the server auto-installs on the first
request (slow, needs network) and the profile is dominated by install work —
`profile.sh` warns when the `.installed` marker is missing. Running the test
suite (or `make deps/tree-sitter`) once populates it.

## Why it's shaped this way

- **Drive synchronously, don't pipe a static session.** The default isolates one
  request at a time; without `--edits`, requests after warmup intentionally
  measure unchanged-snapshot cache hits. Add `--edits 1` to measure the
  edit→reparse→recompute path, or `--burst`/`--burst-edits` to measure
  supersession pressure with completed, cancelled, and `null` latency reported
  separately. (`gen_session.py` can still emit a framed session for other uses.)
- **Profile the server, driven by Python.** The semantic-tokens code is
  `pub(crate)`, so it can't be called from a bench/example. samply launches the
  driver and follows its child (the server), capturing the server's stacks.
- **Record all processes for xctrace.** `xctrace record --launch` follows only
  the Python driver here, not the child server, so `xctrace.sh` records all
  processes for a bounded window and filters the export down to
  `target/profiling/kakehashi`.
- **Symbolicate offline.** samply only symbolicates when serving a profile in the
  browser; the saved JSON keeps raw module-relative addresses. `analyze.py`
  resolves the kakehashi frames with `atos` against the `.dSYM` (built by the
  `profiling` cargo profile + `dsymutil`), so the flow is headless.

## Reading the result

samply samples wall-clock, so blocked syscalls (the synchronous request/response
IO) show up as `[libsystem_kernel.dylib]` — that's IO wait, not compute. Focus on
the kakehashi/tree-sitter/regex frames for the actual CPU cost.

## Files

| file | role |
| ---- | ---- |
| `profile.sh` | end-to-end: build → dSYM → samply record → analyze → SVG |
| `xctrace.sh` | end-to-end: build → Instruments Time Profiler → XML summary |
| `drive.py` | synchronous/batched LSP driver with per-method latency and wire-volume output |
| `test_drive.py` | unit and fake-LSP integration tests for metrics and profiler-handshake ordering |
| `gen_session.py` | document generators + a framed-session emitter |
| `analyze.py` | atos symbolication, self/inclusive report, collapsed stacks |
