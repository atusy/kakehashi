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

## Environment

* Initial cold-start and pilot date: 2026-07-18
* Corrected steady-state collection date: 2026-07-19
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
independent process pairs collected in two 10-pair batches. Cache-hit runs first
issued one unmeasured warmup, then 1,000 measured requests per process;
edit runs issued 100 requests per process. Reported confidence intervals are a
deterministic paired bootstrap over the 20 run-level summaries. Percentiles are
rounded to 0.1 ms by the driver, so sub-0.1-ms percentile differences are below
its reporting resolution.

## Results

### Steady-state request latency

| Scenario | Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|---:|
| Rust small, unchanged cache hit | p50 | 0.40 ms | 0.41 ms | +0.01 ms | [0.00, 0.03] ms |
| Rust small, unchanged cache hit | p95 | 0.45 ms | 0.50 ms | +0.06 ms | [0.04, 0.08] ms |
| Rust small, unchanged cache hit | p99 | 0.49 ms | 0.50 ms | +0.01 ms | [0.00, 0.03] ms |
| Rust small, one edit/request | p50 | 1.64 ms | 1.65 ms | +0.02 ms | [-0.01, 0.04] ms |
| Rust small, one edit/request | p95 | 1.74 ms | 1.76 ms | +0.03 ms | [-0.01, 0.05] ms |
| Rust small, one edit/request | p99 | 1.89 ms | 1.85 ms | -0.04 ms | [-0.22, 0.08] ms |
| Markdown injections, one edit/request | p50 | 3.43 ms | 3.45 ms | +0.03 ms | [-0.05, 0.09] ms |
| Markdown injections, one edit/request | p95 | 3.61 ms | 3.73 ms | +0.12 ms | [0.005, 0.255] ms |
| Markdown injections, one edit/request | p99 | 3.70 ms | 3.95 ms | +0.26 ms | [0.03, 0.61] ms |

The cache-hit p95 and Markdown p95/p99 intervals exclude zero, consistent with
small relay tail costs. The other tail-latency intervals do not distinguish the
relay from run-level scheduling noise at the driver's reporting resolution.
These concrete relay costs are not bounds on the future worker protocol.

### Throughput-sensitive cache-hit path

The cache-hit path transferred approximately 14.0 MiB of response bodies per
1,000-request run.

| Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Wall time / 1,000 requests | 427.4 ms | 441.5 ms | +14.1 ms (+3.3%) | [7.9, 21.7] ms |
| Amortized extra wall time | — | — | +14.1 µs/request | [7.9, 21.7] µs/request |

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
| Rust small | 1486.3 ms | 1512.0 ms | +25.7 ms (+257 µs/cycle) | [16.5, 35.3] ms |
| Markdown injections | 1684.7 ms | 1689.7 ms | +5.1 ms (+51 µs/cycle) | [-7.3, 18.5] ms |

The Rust interval excludes zero, but this experiment cannot attribute the
difference to pipe transport: every cycle includes the fixed edit-settle delay,
parse scheduling, derivation, response relay, and parent/child scheduling. The
real worker benchmark must separate enqueue, queue, compute, serialization, and
resume time before treating this as a protocol cost.

### Fresh-process path

`hyperfine` ran 20 fresh processes per condition, after three warmups, with one
immediate Rust request:

| Path | Mean | Standard deviation | Range |
|---|---:|---:|---:|
| Direct | 87.4 ms | 1.6 ms | 85.9–91.2 ms |
| Relay | 106.7 ms | 1.2 ms | 105.4–111.2 ms |

The observed +19.3 ms includes starting a Python interpreter and is not an
estimate of a Rust worker's spawn/handshake time. It only shows that cold-start
cost is visible and must be measured separately in the Stage 1 prototype.

### Concurrent captures pilot

A single 100-cycle Markdown run queued captures before semantic tokens. It was
used as a smoke test, not an independently repeated result:

| Metric | Direct | Relay |
|---|---:|---:|
| Semantic tokens p50 / p95 | 2.6 / 2.7 ms | 2.6 / 2.8 ms |
| Captures delta p50 / p95 | 16.8 / 17.7 ms | 16.8 / 17.2 ms |

## Interpretation

This particular raw process/pipe relay did not expose a clear steady-state
transport blocker on this machine. Its median and cache-hit p95 deltas were at
or below the driver's 0.1-ms reporting resolution. Markdown edit p95 and p99
were measurable above that resolution at +0.12 ms and +0.26 ms, respectively,
and the cache-hit throughput run estimated about 14.1 microseconds of amortized
extra wall time per request. The actual Stage 1 worker cost may be above or
below these relay deltas.

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
`benches/profile/results/single_worker_phase0_2026-07-18.json`. Recompute every
steady-state table value and confidence interval with:

```sh
python3 benches/profile/analyze_worker_proxy.py
```

The July 19 steady-state section combines two collector batches;
it preserves every run summary and status count but not each raw stderr stream.
The July 18 cold-start and captures-pilot sections predate that recollection.
The cold-start section preserves all 20 timing samples and is recomputed by the
same analyzer; the single captures pilot remains a smoke test only.

Collect a new alternating 10-pair steady-state batch without hand
transcription using:

```sh
python3 benches/profile/collect_worker_proxy.py \
  --bin ./target/release/kakehashi \
  --data-dir ./deps/test/kakehashi \
  --nvim-treesitter-checkout /path/to/pinned/nvim-treesitter \
  --output /tmp/single-worker-phase0.json
```

The collector verified that the measured metadata cache and every installed
query file byte-match
`nvim-treesitter` revision
`4916d6592ede8c07973490d9322f187e07dfefac`. Reconstruct the parser/query
sources at that revision rather than installing from a moving `main` branch:

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
```

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
find ./cache ./parser ./queries -type f -print0 | sort -z | \
  xargs -0 shasum -a 256 | shasum -a 256
```

A representative direct/relay pair is:

```sh
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --data-dir ./deps/test/kakehashi \
  --lang rust --size 15 --requests 100 --edits 1

KAKEHASHI_WORKER_PROXY_BIN=./target/release/kakehashi \
python3 benches/profile/drive.py \
  --bin python3 \
  --server-arg ./benches/profile/worker_proxy.py \
  --data-dir ./deps/test/kakehashi \
  --lang rust --size 15 --requests 100 --edits 1
```
