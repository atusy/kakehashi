# Semantic-tokens profiling harness

Flamegraph profiling of the `semanticTokens/full` hot path, used to find and
verify bottlenecks that the A/B benchmark (`benches/semantic_tokens.rs`) then
quantifies.

> **Flamegraph tooling is macOS only.** Offline symbolication relies on `.dSYM` + `dsymutil`/`atos`,
> and `analyze.py` assumes the macOS `__TEXT` base. `profile.sh` checks for this
> and fails early elsewhere. (The benchmark in `benches/semantic_tokens.rs` is
> cross-platform. The direct driver is also cross-platform; the discovery
> matrix runner below requires POSIX process groups.)

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

For reproducible edit-cycle measurements, exclude warmup and remove the default
10 ms pause after each edit. JSON output preserves individual request samples,
cycle wall times (starting before the edits), binary/fixture hashes, warnings,
and resource usage:

```sh
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi --file path/to/input.md \
  --requests 30 --warmup 5 --edits 1 --edit-delay-ms 0 \
  --json-output /tmp/semantic-run.json
```

Use `--documents 4` to open four copies in one server. Each cycle edits all
copies, then queues one token request per document before reading their responses.
This exercises the shared compute pool without same-document supersession; each
JSON request sample includes its URI. When `--documents` is greater than 1,
it cannot be combined with `--burst` greater than 1, `--captures`, or
`--concurrent-captures`. In this mode cycle time covers the entire batch, not
one document.

The matrix adds `--tag-document-uris`, attaching a distinct
`kakehashi-profile-document` query value to every host URI. Kakehashi preserves
that query in virtual URIs, allowing the controlled peer's observations to be
associated with their host without changing the file directory or basename.
Direct driver invocations keep their existing URIs unless this flag is supplied.

`cycle_seconds` includes all edits, configured edit delays and all responses in
the cycle; it is edit-to-response latency for the single-request `--edits 1`
case. Request samples start at the request itself. CPU and peak RSS describe
reaped child processes over the whole session, including initialization, warmup
and shutdown; they are not steady-state CPU or a sampled memory timeline.
Platforms without Python's `resource` module report these fields as `null`.
Notification counts and server warnings/errors also include initialization and
warmup, so query-loading failures remain visible. Check them before treating a
fast run as evidence of successfully executed work.

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

## Discovery attribution fixtures

`prepare_discovery.py` creates complete Rust and Markdown fixtures near 2 KiB,
32 KiB and 1 MiB, plus native, Lua-only and wildcard bridge configurations. Rust
contains documentation/ordinary comments and macros; Markdown mixes ordinary
prose, inline markup and Lua fences. Edits toggle a character in the first
comment/prose line without breaking the code. Sizes are targets; exact bytes and
hashes are recorded in `inputs.json` together with parser/query and script hashes.

```sh
python3 benches/profile/prepare_discovery.py \
  --runtime /path/to/fixed-runtime --output /tmp/discovery-inputs
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --server-arg=--config-file --server-arg=/tmp/discovery-inputs/narrow.toml \
  --file /tmp/discovery-inputs/markdown-common.md \
  --warmup 5 --requests 30 --edits 1 --edit-delay-ms 0 \
  --json-output /tmp/discovery-run.json
```

The runtime must already contain `parser/` and `queries/`, including the Rust,
Markdown, Markdown-inline, Lua, comment and regex grammars and their real query
corpus. No assets are installed or modified. Use an immutable runtime snapshot
and retain its upstream revisions alongside the generated hash manifest. The
output directory must be new, so old fixtures and peer observations cannot be
silently reused.

Both bridge configurations use `bridge_fixture.py`, a controlled sync-only LSP
peer with no analysis or token provider. The narrow configuration accepts Lua
injections; the wildcard configuration accepts every injection and the host.
This measures kakehashi's routing/synchronization overhead and does not model
EmmyLua or another real analyzer's costs. Before acknowledging shutdown, the peer
records method/language counts and the distinct opened URI/language pairs under
`peer-summaries/`. The matrix requires an injection open for every expected
language from every host document, plus the exact host URI in wildcard mode.
These observations prove coverage during the session, not freshness after each
edit. Keep the per-run files: successful native tokens alone do not prove bridge
coverage, and many regions from one document cannot stand in for another host.

### Alternating matrix runs

After building and retaining exact baseline/candidate binaries, run:

```sh
python3 benches/profile/measure_discovery.py \
  --baseline /path/to/baseline --candidate /path/to/instrumented-candidate \
  --inputs /tmp/discovery-inputs --output /tmp/discovery-results
```

This POSIX runner defaults to both languages, all three sizes/configurations,
1 and 4 documents, and three alternating A/B pairs per scenario. Each run uses
5 warmup and 30 measured edit cycles. Override `--sizes`, `--modes`,
`--documents`, `--requests` and `--repeats` for a smoke test. Logged candidate
runs follow each scenario separately and never enter the A/B latency sample.
Run the authoritative matrix on an otherwise idle machine; retain source
revisions, build commands/toolchain and runtime provenance alongside the output.

`matrix.json` records binary/input/harness hashes, invocation order, peer
observations and completion/failure status. Its global status becomes `complete`
only after final input verification; any run or final-verification failure is
recorded as `failed` with the error. Per-run JSON and stderr are retained,
and runtime verification checks both the parser/query file inventory and hashes.
The `.phases.json` files contain phase calls ending between the driver's measurement
markers. Warmup calls can finish in that window, and final background work can
finish after it; these are invocation distributions, not per-request accounting.
Cancelled/incomplete calls are retained with their outcome details. The runner
fails on missing token work, expected bridge opens, or completed discovery
attribution in the measurement window, and retains partial evidence on failure.
A timeout kills its isolated driver/server process group. Query
warnings remain in each run artifact and must be assessed before drawing a
conclusion. It never edits runtime queries to make a run appear successful.

## Opt-in phase timings

Set `RUST_LOG=kakehashi::profile=debug,kakehashi::semantic=debug` when running
the driver to record phase durations on stderr. The `kakehashi::profile`
timers acquire timestamps only when their Debug target is enabled. Run latency
comparisons separately with logging disabled: output and timestamp collection
perturb the profiled execution.

| phase | measured interval |
| ----- | ----------------- |
| `parse` | parser work including incremental-seed replay, excluding pool queue and parser checkout; retries have separate `attempt` values |
| `parse_await` | pool submission through async resumption, including checkout, parse attempts and queue wait |
| `discovery` | the full-tree injection query traversal |
| `resolution` | resolving collected regions into languages and virtual content, when bridge regions are required |
| `snapshot_wait` | semantic-token request waiting for a current snapshot, including cancellation/supersession outcomes |

Durations are wall time in microseconds, not CPU time. `completed=false` marks
a call that returned no result; it does not distinguish cancellation from parser
unavailability. Dropping a parse future before it resumes yields no
`parse_await` record. Missing injection queries skip discovery, and empty regions
or bridge-less configurations skip resolution; a missing record is not a zero
cost measurement. The timings exclude region bookkeeping outside those calls.

The existing `kakehashi::semantic` phase log reports host tokenization, injection
tokenization, finalization and total compute. These intervals overlap: parser
work is inside `parse_await`, snapshot waits can overlap parsing/discovery, and
host/injection tokenization may run in parallel. Do not sum their medians into
an end-to-end latency. Compare them with separately measured edit-to-response
latency and sampled CPU/resource usage.

## Files

| file | role |
| ---- | ---- |
| `profile.sh` | end-to-end: build → dSYM → samply record → analyze → SVG |
| `xctrace.sh` | end-to-end: build → Instruments Time Profiler → XML summary |
| `drive.py` | synchronous/batched LSP driver with per-method latency and wire-volume output |
| `test_drive.py` | unit tests for driver metric aggregation |
| `gen_session.py` | document generators + a framed-session emitter |
| `analyze.py` | atos symbolication, self/inclusive report, collapsed stacks |
