# Single Tree Worker Phase 0 Measurement

## Scope

This experiment records one concrete feasibility datapoint for adding a
resident child-process transport hop before the Stage 1 tree-worker prototype
exists. It does **not** measure the proposed worker architecture or establish
that it improves performance.

The direct and relayed paths were:

```text
direct: driver -> kakehashi
relay:  driver -> resident Python byte relay -> kakehashi
```

The relay forwards the existing framed LSP byte stream without decoding it. It
therefore measures an extra process, two extra pipe crossings, scheduling, and
byte copying. It does not include the future worker protocol's encoding,
document replica, configuration fences, queueing, hazard handshakes, or movement
of Tree-sitter computation and caches.

This Phase 0 relay is POSIX-only because bounded descendant cleanup depends on
POSIX signals and process groups. The production worker remains cross-platform
and must use the platform-specific lifecycle mechanisms specified by the ADR.

## Environment

* Final fail-closed collection date: 2026-07-19
* Machine: Apple M4, 10 physical/logical CPUs
* OS: macOS 26.5.1
* Estimated tree compute budget under the current policy: 8 threads. This uses
  Python's logical CPU count and is not a binary-reported effective pool size.
* Attested source commit: `d5f1264bf38b7261b7edc02cb53f7b9f7edadf0a`;
  measured binary SHA-256:
  `56cd092bbbb8bb6729d478be43500bd41f696f6ba75bea4c46c628b7287658ba`.
  Production Rust source and Cargo build inputs match `origin/main` at
  `a1278be5fdff24d109d9e03134c6bdb880577f64`; the intervening branch changes
  are benchmark tooling, tests, workflow, and documentation only. The committed
  attestation archives the clean attested commit with Git replacement objects
  and ambient system/global Git configuration disabled, to
  a temporary source root outside the checkout hierarchy, then rebuilds it in
  fresh target and Cargo-home directories with an allowlisted, recorded build
  environment. Before building, it rejects a temporary source inside the
  checkout or beneath any ancestor Cargo configuration. It also fetches the
  recorded origin into an isolated bare repository and requires the source
  commit to be reachable from a fetched remote branch. It records the
  Rust/Cargo versions, verbose Rust host/LLVM metadata, native compiler and
  linker identities, macOS SDK path/version, and locked build command, and
  matches the measured binary's SHA-256 digest.
* Parser/query data was preinstalled outside the measured interval.
* Each collector copied the attested binary, digest-verified runtime tree, and
  driver/relay/helper scripts to a private temporary directory before warmup.
  Measured children executed those staged inputs; all collector and helper
  script digests were recorded, and original/staged scripts were rechecked
  before accepting the run.
* The collector removed ambient Rust/kakehashi behavior overrides and retained
  only the recorded path, temporary-directory, locale, and loader variables.

For each steady-state scenario, the harness first ran one unmeasured direct and
relay process, then alternated order across 20 independent measured process
pairs. Cache-hit processes additionally issued one in-process unmeasured warmup,
then 1,000 measured requests; edit processes issued 100 measured requests.
Reported confidence intervals are a deterministic paired bootstrap over the 20
run-level summaries. Percentiles are rounded to 0.1 ms by the driver, so
sub-0.1-ms percentile differences are below its reporting resolution.
End-to-end wall times retain the floating-point `perf_counter` result without
millisecond rounding before analysis.

## Results

### Steady-state request latency

| Scenario | Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|---:|
| Rust small, unchanged cache hit | p50 | 0.585 ms | 0.630 ms | +0.045 ms | [0.010, 0.085] ms |
| Rust small, unchanged cache hit | p95 | 0.625 ms | 0.680 ms | +0.055 ms | [0.025, 0.090] ms |
| Rust small, unchanged cache hit | p99 | 0.630 ms | 0.735 ms | +0.105 ms | [0.075, 0.135] ms |
| Rust small, one edit/request | p50 | 2.150 ms | 2.060 ms | -0.090 ms | [-0.220, 0.025] ms |
| Rust small, one edit/request | p95 | 2.410 ms | 2.305 ms | -0.105 ms | [-0.250, 0.030] ms |
| Rust small, one edit/request | p99 | 2.615 ms | 2.690 ms | +0.075 ms | [-0.320, 0.525] ms |
| Markdown injections, one edit/request | p50 | 4.675 ms | 4.920 ms | +0.245 ms | [-0.005, 0.495] ms |
| Markdown injections, one edit/request | p95 | 5.220 ms | 5.595 ms | +0.375 ms | [-0.270, 0.970] ms |
| Markdown injections, one edit/request | p99 | 5.585 ms | 6.675 ms | +1.090 ms | [0.095, 2.175] ms |

Batch A's cache-hit percentile intervals exclude zero, although p50/p95 point
deltas are below the driver's 0.1-ms reporting resolution and p99 is at that
scale. Batch B reported +0.085 ms [0.055, 0.115] at p50, +0.065 ms
[0.030, 0.105] at p95, and +0.060 ms [0.010, 0.110] at p99. The edit
percentile effects change across batches. These rounded percentile results do
not bound the future worker protocol.

For Rust edit latency, batch B measured p50/p95/p99 deltas of +0.045 ms
[-0.035, 0.135], -0.015 ms [-0.130, 0.100], and -0.500 ms [-1.150, 0.010].
For Markdown, batch B measured -0.070 ms [-0.250, 0.120], -0.175 ms
[-0.910, 0.475], and -0.645 ms [-1.610, 0.230], reversing batch A's positive
point estimates. Every batch B edit interval crosses zero. This batch
dependence is why neither edit result is treated as a stable relay effect.

### Throughput-sensitive cache-hit path

The cache-hit path transferred approximately 14.0 MiB of response bodies per
1,000-request run.

| Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Wall time / 1,000 requests | 878.1 ms | 923.6 ms | +45.5 ms (+5.2%) | [9.5, 84.4] ms |
| Amortized wall-time delta | — | — | +45.5 µs/request | [9.5, 84.4] µs/request |

This is the most sensitive raw-relay estimate in batch A. Batch B measured a
same-direction +68.4-ms delta [33.2, 105.3]. Both independently collected
intervals exclude zero. This observed Python-relay cost is neither a lower nor
an upper bound for
the future worker transport: the Python relay adds interpreter, threads, an
extra buffering boundary, and flush costs, while the real protocol adds
different payloads, encoding, queueing, and scheduling. The edit scenarios
include a fixed 10-ms delay after each edit, so their total wall times
intentionally cannot isolate a similarly small transport cost. Their observed
end-to-end cycle times are nevertheless disclosed below.

### End-to-end edit cycles

| Scenario | Direct / 100 cycles | Relay / 100 cycles | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Rust small | 1580.4 ms | 1569.7 ms | -10.7 ms (-107 µs/cycle) | [-27.1, 4.3] ms |
| Markdown injections | 1861.8 ms | 1915.4 ms | +53.6 ms (+536 µs/cycle) | [8.2, 99.4] ms |

Batch B measured Rust at -6.6 ms [-22.8, 10.0] and Markdown at -1.6 ms
[-39.0, 36.0]. The apparent Markdown regression in batch A reverses and does
not repeat.
This experiment cannot attribute either difference to pipe transport: every cycle includes the fixed edit-settle delay,
parse scheduling, derivation, response relay, and parent/child scheduling. The
real worker benchmark must separate enqueue, queue, compute, serialization, and
resume time before treating them as protocol costs.

### Validated fresh-process probe

The bounded collector ran 20 alternating direct/relay process pairs after three
warmup pairs, with one immediate validated Rust request:

| Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---:|---:|---:|---:|
| 131.0 ms | 170.6 ms | +39.6 ms | [36.1, 44.8] ms |

The relay series includes Python interpreter startup and is not an estimate of
a Rust worker's spawn/handshake time. Stage 1 must repeat this measurement with
the real worker executable and protocol.

### Concurrent captures pilot

A single 100-cycle Markdown run queued captures before semantic tokens, with
each timer starting before request serialization, pipe write, and flush. It was
used as a smoke test, not an independently repeated result:

| Metric | Direct | Relay |
|---|---:|---:|
| Semantic tokens p50 / p95 | 4.8 / 5.6 ms | 5.7 / 6.8 ms |
| Captures delta p50 / p95 | 32.8 / 38.8 ms | 36.8 / 42.9 ms |

All 100 semantic and 100 capture-delta responses per path were successful. The
final driver validated every capture result as the delta `edits` shape and an
advancing `resultId` lineage; no full fallback occurred. The dataset retains
both methods' outcome and fallback counts.

## Interpretation

This raw process/pipe relay did not expose a prohibitive steady-state transport
cost on this machine. The most sensitive cache-hit workload measured +45.5
microseconds/request in batch A and +68.4 microseconds/request in batch B; both
intervals excluded zero. Edit-cycle effects changed across batches, and rounded
request percentiles did not establish a stable tail effect. This front-of-server
relay therefore provides no measured performance improvement, but does provide
a reproducible raw-relay overhead baseline. It cannot locate either cost or
benefit in tree work.

The Stage 1 benchmark must therefore report transport enqueue/copy time and
worker compute/queue/resume time separately and must include independent batch
repetition. A single paired batch, even with a narrow bootstrap interval, is not
an acceptable performance gate. The actual Stage 1 worker cost or benefit may
be above or below any one relay delta.

This result is preliminary and insufficient evidence for the ADR. It does not
show a worker performance improvement, and it cannot detect costs or benefits
from:

* encoding and decoding the actual versioned worker protocol;
* maintaining duplicated authoritative document text;
* worker queue wait and parent resume scheduling;
* fused tree derivation, worker-local caches, or removal of parent-side locks;
* multi-document fairness, obsolete-work cancellation, or internal threading;
* worker memory, cold start, crash recovery, and full resynchronization; or
* hazard and native-segment control handshakes.

The result does not provide a transport-based reason to reject Stage 1, but it
does not justify the architecture either. Stage 1 remains necessary to measure
the actual protocol and tree-data-plane boundary. The ADR must remain
`proposed` until the real `DeriveSnapshot` prototype measures the complete gate
matrix.

## Reproduction

The tail-percentile driver and relay are in `benches/profile/drive.py` and
`benches/profile/worker_proxy.py`. The 20 paired run summaries, execution order,
scenario arguments, environment, and artifact-tree digest are committed in
`benches/profile/results/single_worker_phase0_2026-07-19.json`. The independent
20-pair repeat is in
`benches/profile/results/single_worker_phase0_repeat_2026-07-19.json`. The
separately generated cold-start and captures results are in
`benches/profile/results/single_worker_phase0_cold_start_2026-07-19.json` and
`benches/profile/results/single_worker_phase0_captures_pilot_2026-07-19.json`.
The binary build attestation used by all three collectors is in
`benches/profile/results/single_worker_phase0_binary_attestation_2026-07-19.json`.
Recompute every steady-state table value and confidence interval with:

```sh
python3 benches/profile/analyze_worker_proxy.py
python3 benches/profile/analyze_worker_proxy.py \
  benches/profile/results/single_worker_phase0_repeat_2026-07-19.json
python3 benches/profile/analyze_worker_proxy.py --drop-first-pairs 1 \
  | jq '.rust_edit.p99'
python3 benches/profile/analyze_worker_proxy.py --last-pairs 10 \
  | jq '.rust_edit.p99'
```

The July 19 steady-state section comes from two independent 20-pair collector
runs and preserves every run summary and status count but not each raw stderr
stream. The tables report batch A and the prose discloses batch B; they are not
silently pooled across the observed non-stationarity.
Cold start was recollected as 20 alternating pairs with
`collect_worker_cold_start.py`; every process is bounded and its semantic
response validated. The captures pilot was rerun with
`collect_worker_capture_pilot.py`, which fails unless both
methods return 100 successful responses and the driver validates every delta
shape and advancing lineage. It remains a single-pair smoke test rather than an
independently repeated result.

Reconstruct a dedicated parser/query tree at the pinned revision rather than
installing from a moving `main` branch, then collect a new alternating 10-pair
steady-state batch without hand transcription:

```bash
(
set -euo pipefail
revision=4916d6592ede8c07973490d9322f187e07dfefac
workspace_dir="$(mktemp -d "${TMPDIR:-/tmp}/kakehashi-phase0.XXXXXX")"
test -n "$workspace_dir"
test -d "$workspace_dir"
trap 'rm -rf "$workspace_dir"' EXIT
source_dir="$workspace_dir/nvim-treesitter"
data_dir="$workspace_dir/data"
results_dir="$workspace_dir/results"
mkdir -p "$results_dir"
git clone --filter=blob:none \
  https://github.com/nvim-treesitter/nvim-treesitter "$source_dir"
git -C "$source_dir" checkout "$revision"
python3 benches/profile/attest_worker_binary.py \
  --checkout . \
  --output "$results_dir/single-worker-binary-attestation.json"
mkdir -p "$data_dir/cache" "$data_dir/queries"
cp "$source_dir/lua/nvim-treesitter/parsers.lua" "$data_dir/cache/parsers.lua"
for lang in comment lua markdown markdown_inline python rust yaml; do
  ./target/release/kakehashi language install "$lang" \
    --data-dir "$data_dir" --force
  rm -rf "$data_dir/queries/$lang"
  cp -R "$source_dir/runtime/queries/$lang" "$data_dir/queries/$lang"
done

python3 benches/profile/collect_worker_proxy.py \
  --bin ./target/release/kakehashi \
  --binary-attestation "$results_dir/single-worker-binary-attestation.json" \
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --output "$results_dir/single-worker-phase0.json"
python3 benches/profile/collect_worker_cold_start.py \
  --bin ./target/release/kakehashi \
  --binary-attestation "$results_dir/single-worker-binary-attestation.json" \
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --pairs 20 --warmups 3 \
  --output "$results_dir/single-worker-phase0-cold-start.json"
python3 benches/profile/collect_worker_capture_pilot.py \
  --bin ./target/release/kakehashi \
  --binary-attestation "$results_dir/single-worker-binary-attestation.json" \
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --output "$results_dir/single-worker-phase0-captures-pilot.json"

(cd "$data_dir" && \
  find ./cache ./parser ./queries -type f -print0 | LC_ALL=C sort -z | \
  xargs -0 shasum -a 256 | shasum -a 256)

python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --data-dir "$data_dir" \
  --lang rust --size 15 --requests 100 --edits 1

KAKEHASHI_WORKER_PROXY_BIN=./target/release/kakehashi \
python3 benches/profile/drive.py \
  --bin python3 \
  --server-arg ./benches/profile/worker_proxy.py \
  --data-dir "$data_dir" \
  --lang rust --size 15 --requests 100 --edits 1
)
```

The checkout supplies only the selected revision and declared origin. The
collector fetches the hard-coded official HTTPS `main` into a fresh bare
repository with local/system Git configuration and replacement objects
disabled, requires the selected revision to be in that isolated history, and
reads the expected metadata/query blobs from that same isolated repository.
Every staged metadata and query file must byte-match those official blobs;
compiled parser libraries are covered by the staged tree digest, not upstream
blob comparison. The committed dataset was verified against `nvim-treesitter`
revision
`4916d6592ede8c07973490d9322f187e07dfefac`.

Run the digest command in the recipe above and compare it with the committed dataset before
using a reconstructed tree for comparison. Compiler and platform differences
can change shared-library bytes even from identical grammar revisions, so the
committed digest remains the identity check for the exact measured artifacts.

The analyzer uses seed `123456789`, 100,000 paired-bootstrap resamples, and
nearest-rank 2.5th/97.5th percentiles. Before cleanup, the recipe computes the
parser/query tree digest and runs a representative direct/relay pair. The
digest covers only the runtime `cache`, `parser`, and `queries` roots, excluding
setup markers and unrelated `query-assets`, with paths relative to the data
directory.
