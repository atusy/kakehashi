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
* OS: macOS 26.5.1 (25F80)
* Estimated tree compute budget under the current policy: 8 threads. This uses
  Python's logical CPU count and is not a binary-reported effective pool size.
* Release binary: `ccbd8ffd13c4817eda62e1de6f8cfd3eeb3259d0`
  (execution code matches `origin/main` at
  `a1278be5fdff24d109d9e03134c6bdb880577f64`; the intervening change is
  documentation only). The committed attestation rebuilds this clean checkout,
  in a fresh target directory with an allowlisted, recorded build environment;
  it records the toolchain and build command and matches the measured binary's
  SHA-256 digest.
* Parser/query data was preinstalled outside the measured interval.
* Each collector copied the attested binary and digest-verified runtime tree to
  a private temporary directory before warmup, then executed only those staged
  inputs for the complete collection.
* The collector removed ambient Rust/kakehashi behavior overrides and retained
  only the recorded path, temporary-directory, locale, and loader variables.

For each steady-state scenario, direct and relay order alternated across 20
independent process pairs collected by the final fail-closed harness. Cache-hit runs first
issued one unmeasured warmup, then 1,000 measured requests per process;
edit runs issued 100 requests per process. Reported confidence intervals are a
deterministic paired bootstrap over the 20 run-level summaries. Percentiles are
rounded to 0.1 ms by the driver, so sub-0.1-ms percentile differences are below
its reporting resolution.

## Results

### Steady-state request latency

| Scenario | Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|---:|
| Rust small, unchanged cache hit | p50 | 0.505 ms | 0.500 ms | -0.005 ms | [-0.015, 0.000] ms |
| Rust small, unchanged cache hit | p95 | 0.545 ms | 0.580 ms | +0.035 ms | [0.005, 0.065] ms |
| Rust small, unchanged cache hit | p99 | 0.590 ms | 0.595 ms | +0.005 ms | [-0.050, 0.045] ms |
| Rust small, one edit/request | p50 | 1.800 ms | 1.805 ms | +0.005 ms | [-0.010, 0.020] ms |
| Rust small, one edit/request | p95 | 1.885 ms | 1.905 ms | +0.020 ms | [-0.025, 0.070] ms |
| Rust small, one edit/request | p99 | 2.030 ms | 1.985 ms | -0.045 ms | [-0.180, 0.055] ms |
| Markdown injections, one edit/request | p50 | 3.900 ms | 3.930 ms | +0.030 ms | [-0.050, 0.115] ms |
| Markdown injections, one edit/request | p95 | 4.280 ms | 4.360 ms | +0.080 ms | [-0.015, 0.190] ms |
| Markdown injections, one edit/request | p99 | 4.440 ms | 4.640 ms | +0.200 ms | [-0.040, 0.555] ms |

The cache-hit p95 interval excludes zero when rounded values are treated as
exact observations, but its +0.035-ms estimate is smaller than the driver's
0.1-ms reporting resolution. It therefore does not establish a non-zero tail
effect. Every other tail-latency interval crosses zero. These concrete relay
results are not bounds on the future worker protocol.

### Throughput-sensitive cache-hit path

The cache-hit path transferred approximately 14.0 MiB of response bodies per
1,000-request run.

| Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Wall time / 1,000 requests | 743.0 ms | 745.0 ms | +2.1 ms (+0.3%) | [-16.2, 15.7] ms |
| Amortized extra wall time | — | — | +2.1 µs/request | [-16.2, 15.7] µs/request |

This is the most sensitive raw-relay estimate in this experiment. It is neither
a lower nor an upper bound for the future worker transport: the Python
relay adds interpreter, thread, and flush costs, while the real protocol adds
different payloads, encoding, queueing, and scheduling. The edit scenarios
include a fixed 10-ms delay after each edit, so their total wall times
intentionally cannot isolate a similarly small transport cost. Their observed
end-to-end cycle times are nevertheless disclosed below.

### End-to-end edit cycles

| Scenario | Direct / 100 cycles | Relay / 100 cycles | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Rust small | 1519.2 ms | 1540.8 ms | +21.7 ms (+217 µs/cycle) | [9.2, 34.8] ms |
| Markdown injections | 1835.5 ms | 1814.2 ms | -21.4 ms (-214 µs/cycle) | [-126.5, 38.0] ms |

The Rust interval excludes zero, but this experiment cannot attribute the
difference to pipe transport: every cycle includes the fixed edit-settle delay,
parse scheduling, derivation, response relay, and parent/child scheduling. The
real worker benchmark must separate enqueue, queue, compute, serialization, and
resume time before treating this as a protocol cost.

### Validated fresh-process probe

The bounded collector ran 20 alternating direct/relay process pairs after three
warmup pairs, with one immediate validated Rust request:

| Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---:|---:|---:|---:|
| 156.7 ms | 200.5 ms | +43.8 ms | [37.6, 53.1] ms |

The relay series includes Python interpreter startup and is not an estimate of
a Rust worker's spawn/handshake time. Stage 1 must repeat this measurement with
the real worker executable and protocol.

### Concurrent captures pilot

A single 100-cycle Markdown run queued captures before semantic tokens. It was
used as a smoke test, not an independently repeated result:

| Metric | Direct | Relay |
|---|---:|---:|
| Semantic tokens p50 / p95 | 5.8 / 7.2 ms | 4.9 / 5.8 ms |
| Captures delta p50 / p95 | 37.3 / 41.6 ms | 30.4 / 38.4 ms |

All 100 semantic and 100 capture-delta responses per path were successful. The
final driver validated every capture result as the delta `edits` shape and an
advancing `resultId` lineage; no full fallback occurred. The dataset retains
both methods' outcome and fallback counts.

## Interpretation

This particular raw process/pipe relay did not expose a clear steady-state
transport blocker on this machine. Request-tail point deltas were below the
driver's 0.1-ms reporting resolution or had intervals crossing zero. The
cache-hit throughput run estimated 2.1 microseconds of amortized extra wall time
per request with a [-16.2, 15.7]-microsecond interval for this concrete relay. The
actual Stage 1 worker cost may be above or below these relay deltas.

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
`benches/profile/results/single_worker_phase0_2026-07-19.json`. The separately
generated cold-start and captures results are in
`benches/profile/results/single_worker_phase0_cold_start_2026-07-19.json` and
`benches/profile/results/single_worker_phase0_captures_pilot_2026-07-19.json`.
The binary build attestation used by both collectors is in
`benches/profile/results/single_worker_phase0_binary_attestation_2026-07-19.json`.
Recompute every steady-state table value and confidence interval with:

```sh
python3 benches/profile/analyze_worker_proxy.py
```

The July 19 steady-state section comes from one final 20-pair collector run and
preserves every run summary and status count but not each raw stderr stream.
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

```sh
revision=4916d6592ede8c07973490d9322f187e07dfefac
source_dir=/tmp/kakehashi-nvim-treesitter
data_dir=./deps/profile/kakehashi
git clone --filter=blob:none \
  https://github.com/nvim-treesitter/nvim-treesitter "$source_dir"
git -C "$source_dir" checkout "$revision"
python3 benches/profile/attest_worker_binary.py \
  --checkout . \
  --output /tmp/single-worker-binary-attestation.json
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
  --binary-attestation /tmp/single-worker-binary-attestation.json \
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --output /tmp/single-worker-phase0.json
python3 benches/profile/collect_worker_cold_start.py \
  --bin ./target/release/kakehashi \
  --binary-attestation /tmp/single-worker-binary-attestation.json \
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --pairs 20 --warmups 3 \
  --output /tmp/single-worker-phase0-cold-start.json
python3 benches/profile/collect_worker_capture_pilot.py \
  --bin ./target/release/kakehashi \
  --binary-attestation /tmp/single-worker-binary-attestation.json \
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --output /tmp/single-worker-phase0-captures-pilot.json
```

The collector verifies that the metadata cache and every installed query file
byte-match the checkout's HEAD. The committed dataset was verified against
`nvim-treesitter` revision
`4916d6592ede8c07973490d9322f187e07dfefac`.

Run the digest command below and compare it with the committed dataset before
using a reconstructed tree for comparison. Compiler and platform differences
can change shared-library bytes even from identical grammar revisions, so the
committed digest remains the identity check for the exact measured artifacts.

The analyzer uses seed `123456789`, 100,000 paired-bootstrap resamples, and
nearest-rank 2.5th/97.5th percentiles. The parser/query tree digest covers only
the runtime `cache`, `parser`, and `queries` roots, excluding setup markers and
unrelated `query-assets`. It was computed from paths relative to the data
directory:

```sh
(cd "$data_dir" && \
  find ./cache ./parser ./queries -type f -print0 | sort -z | \
  xargs -0 shasum -a 256 | shasum -a 256)
```

A representative direct/relay pair is:

```sh
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
```
