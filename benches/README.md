# Semantic token measurements

`semantic_tokens.rs` drives built server binaries over LSP. It includes cache,
range, cold-open, unique-edit typing, eight-edit bursts, and cancellation cases.
Typing responses reconstruct the client delta baseline and check a marker's
expected position. This detects stale responses even when `resultId` changes;
it is not a full-document semantic correctness oracle.

Run an exploratory scenario with `cargo bench --bench semantic_tokens --features
e2e`, selecting scenarios with `KAKEHASHI_BENCH_SCENARIOS`. For comparisons:

```sh
python3 benches/collect_semantic_pairs.py \
  origin/main candidate-ref "$HOME/.local/share/kakehashi-benchmarks/comparison"
```

The collector requires a clean committed checkout, POSIX, Python 3.10+, Git,
Cargo, and the C toolchain used by the parser installer. It builds both exact
source refs and a separate harness, then runs four alternating AB/BA pairs with
six warmups and 30 samples. Builds and servers use an allowlisted environment.
The same installer-created parser/query tree is frozen for both binaries.

The output directory must be absent or empty. Results are published atomically
after validation. The manifest records source, binary, harness, runtime, fixture,
toolchain, and sample hashes. Keep raw samples and parser/query assets outside
Git; commit only a compact report and provenance needed to reproduce it.
Summaries report paired median differences and ranges; p95 is descriptive.
The exploratory harness may retry cancellation attempts that finish before
cancellation. The paired collector deliberately rejects runs with any such
discarded attempts: inspect that race separately instead of automatically
comparing latency distributions conditioned on different retained attempts.

Timing boundaries: full/cache-hit, range, cold-open, and cancellation samples
end at response receipt, before token validation. Delta scenarios, including
no-op deltas and ordinary typing, include local baseline reconstruction and
validation. Invalid responses fail collection in either case. Eight-edit typing
queues edits before one request; cancellation cases exercise in-flight work.
Neither measures a real editor's request scheduling or highlight rendering.
Do not label these samples as time to visible highlight application.

`profile/neovim_typing.lua` records a real Neovim client's edit, requests,
refreshes, cancellations, responses, and completed token conversion. Run it in
an isolated `nvim --clean --headless -l benches/profile/neovim_typing.lua` process
with `NVIM_LISTEN_ADDRESS` unset. Set `PROBE_BIN`, `PROBE_FILE`, `PROBE_CONFIG`,
`PROBE_OUTPUT`, and `KAKEHASHI_DATA_DIR` to the exact binary, fixture, config,
private JSON output, and runtime used by the experiment. `PROBE_SAMPLES` defaults
to eight; `PROBE_LINE` defaults to zero-based line one. Choose a token-bearing
line where inserted leading spaces only move its first token, such as a Rust
`use` statement. Each completed sample must have a full/delta result that moves that token to
the latest unique position. The file is edited in memory and never saved.
Set `PROBE_BURST=8` for eight edits per sample and
`PROBE_INTERVAL_MS=20` for their spacing. `follow_ms` measures from the last edit;
`ready_ms` measures from the start of the burst. The final token array's SHA-256
supports an untimed full-array equality checkpoint across the two binaries.

The probe uses private Neovim semantic-token state, tested with 0.13-dev, and
fails if the API or marker contract is unavailable. Read the ordered `events`
array to distinguish an immediate first request from a later refresh-induced
replacement; the scalar request/response timestamps describe the last events.
`wire_response` records null results and errors delivered to Neovim's request
callback; Neovim filters `RequestCancelled` acknowledgments before that callback.
`wire_reply` records reply arrival before this filter, including cancellation
acknowledgments, but exposes only the request ID, not the response payload.
On an edit timeout, the probe exits unsuccessfully and writes a partial trace
with an `error` field and `failed_sample`. It is diagnostic evidence, not a
completed benchmark run.
`ready_ms` means full/delta token conversion completed, not that a screen frame
was painted. This probe complements the isolated LSP comparisons.

The baseline validation and paired collector were recovered from unmerged
PR #899 (`15af9f9c1f43d9bd826a6e04ae889a28c15e8375`). No experimental server
changes from that branch are included.
