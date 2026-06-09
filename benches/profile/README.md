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

The harness drives the server against `deps/test/kakehashi` for parsers/queries.
If that dir has no installed parsers, the server auto-installs on the first
request (slow, needs network) and the profile is dominated by install work —
`profile.sh` warns when the `.installed` marker is missing. Running the test
suite (or `make deps/tree-sitter`) once populates it.

## Why it's shaped this way

- **Drive synchronously, don't pipe a static session.** The server coalesces
  superseded requests: if many `semanticTokens/full` calls are queued at once it
  answers all but the last with `-32800 Canceled` and does no real work. `drive.py`
  waits for each response before sending the next, so every request actually
  computes. (`gen_session.py` can still emit a framed session for other uses.)
- **Profile the server, driven by Python.** The semantic-tokens code is
  `pub(crate)`, so it can't be called from a bench/example. samply launches the
  driver and follows its child (the server), capturing the server's stacks.
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
| `drive.py` | synchronous LSP driver (no request coalescing) |
| `gen_session.py` | document generators + a framed-session emitter |
| `analyze.py` | atos symbolication, self/inclusive report, collapsed stacks |
