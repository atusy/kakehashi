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
  documentation only)
* Parser/query data was preinstalled outside the measured interval.
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
| Rust small, unchanged cache hit | p50 | 0.495 ms | 0.500 ms | +0.005 ms | [0.000, 0.015] ms |
| Rust small, unchanged cache hit | p95 | 0.500 ms | 0.515 ms | +0.015 ms | [0.000, 0.030] ms |
| Rust small, unchanged cache hit | p99 | 0.520 ms | 0.520 ms | 0.000 ms | [-0.040, 0.030] ms |
| Rust small, one edit/request | p50 | 1.710 ms | 1.725 ms | +0.015 ms | [-0.010, 0.040] ms |
| Rust small, one edit/request | p95 | 1.785 ms | 1.845 ms | +0.060 ms | [0.020, 0.095] ms |
| Rust small, one edit/request | p99 | 1.885 ms | 1.905 ms | +0.020 ms | [-0.035, 0.075] ms |
| Markdown injections, one edit/request | p50 | 3.735 ms | 3.785 ms | +0.050 ms | [-0.005, 0.110] ms |
| Markdown injections, one edit/request | p95 | 3.925 ms | 3.950 ms | +0.025 ms | [-0.045, 0.095] ms |
| Markdown injections, one edit/request | p99 | 4.015 ms | 4.735 ms | +0.720 ms | [-0.015, 2.015] ms |

Only the Rust edit p95 interval excludes zero, consistent with a small relay
tail cost. Markdown p99 has a larger +0.720-ms point estimate but a wide interval
that crosses zero. The other tail-latency intervals do not distinguish the relay
from run-level scheduling noise. These concrete relay results are not bounds on
the future worker protocol.

### Throughput-sensitive cache-hit path

The cache-hit path transferred approximately 14.0 MiB of response bodies per
1,000-request run.

| Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Wall time / 1,000 requests | 704.3 ms | 714.6 ms | +10.4 ms (+1.5%) | [-0.5, 23.9] ms |
| Amortized extra wall time | — | — | +10.4 µs/request | [-0.5, 23.9] µs/request |

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
| Rust small | 1506.9 ms | 1533.6 ms | +26.7 ms (+267 µs/cycle) | [16.5, 36.9] ms |
| Markdown injections | 1821.8 ms | 1779.5 ms | -42.3 ms (-423 µs/cycle) | [-154.9, 20.8] ms |

The Rust interval excludes zero, but this experiment cannot attribute the
difference to pipe transport: every cycle includes the fixed edit-settle delay,
parse scheduling, derivation, response relay, and parent/child scheduling. The
real worker benchmark must separate enqueue, queue, compute, serialization, and
resume time before treating this as a protocol cost.

### Non-comparative fresh-process observations

`hyperfine` ran 20 direct processes followed by 20 relay processes, after three
warmups per condition, with one immediate Rust request:

| Path | Mean | Standard deviation | Range |
|---|---:|---:|---:|
| Direct | 123.4 ms | 10.1 ms | 101.2–132.9 ms |
| Relay | 156.9 ms | 6.0 ms | 147.3–167.1 ms |

The series were not interleaved: direct timings rose during their series while
relay timings fell during theirs. They are retained as environment-specific
observations, not as a comparative delta. The relay series also includes Python
interpreter startup and is not an estimate of a Rust worker's spawn/handshake
time. Stage 1 must measure cold start with alternating pairs.

### Concurrent captures pilot

A single 100-cycle Markdown run queued captures before semantic tokens. It was
used as a smoke test, not an independently repeated result:

| Metric | Direct | Relay |
|---|---:|---:|
| Semantic tokens p50 / p95 | 4.4 / 5.2 ms | 4.4 / 5.0 ms |
| Captures delta p50 / p95 | 29.8 / 32.1 ms | 29.3 / 32.3 ms |

All 100 semantic and 100 capture-delta responses per path were successful. The
final driver validated every capture result shape and advancing `resultId`
lineage; the dataset retains both methods' outcome counts.

## Interpretation

This particular raw process/pipe relay did not expose a clear steady-state
transport blocker on this machine. Its median and p95 point deltas were at or
below the driver's 0.1-ms reporting resolution; Rust edit p95 nevertheless had
an interval above zero. Markdown edit p99 had a +0.720-ms point estimate with an
interval crossing zero. The cache-hit throughput run estimated about 10.4
microseconds of amortized extra wall time per request, also with an interval
crossing zero. The actual Stage 1 worker cost may be above or below these relay
deltas.

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
commands, environment, artifact-tree digest, cold-start result, and captures
pilot are committed in
`benches/profile/results/single_worker_phase0_2026-07-19.json`. Recompute every
steady-state table value and confidence interval with:

```sh
python3 benches/profile/analyze_worker_proxy.py
```

The July 19 steady-state section comes from one final 20-pair collector run and
preserves every run summary and status count but not each raw stderr stream.
Cold start was recollected under the same controlled environment and artifact
identity; all 20 timing samples per non-interleaved condition are recomputed by
the analyzer but are explicitly non-comparative. The captures
pilot was also rerun with the final harness, but remains a single-pair smoke
test rather than an independently repeated result.

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
  --data-dir "$data_dir" \
  --nvim-treesitter-checkout "$source_dir" \
  --output /tmp/single-worker-phase0.json
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
