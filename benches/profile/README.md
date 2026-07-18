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

The driver reports per-method p50/p90/p95/p99/max request-to-response latency both
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
| `test_drive.py` | unit tests for driver metric aggregation |
| `attest_worker_binary.py` | isolated source-archive release build and binary/source attestation |
| `collect_worker_cold_start.py` | bounded alternating fresh-process direct/relay collector |
| `collect_worker_capture_pilot.py` | fail-closed direct/relay concurrent-captures smoke collector |
| `gen_session.py` | document generators + a framed-session emitter |
| `analyze.py` | atos symbolication, self/inclusive report, collapsed stacks |
