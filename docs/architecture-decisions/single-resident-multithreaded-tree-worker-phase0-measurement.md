# Single Tree Worker Phase 0 Measurement

## Scope

This experiment measures a lower bound for adding one resident child-process
transport hop before the Stage 1 tree-worker prototype exists. It does **not**
measure the proposed worker architecture or establish that it improves
performance.

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

* Date: 2026-07-18
* Machine: Apple M4, 10 physical/logical CPUs
* OS: macOS 26.5.1 (25F80)
* Current tree compute budget: 8 threads
* Release binary: `ccbd8ffd13c4817eda62e1de6f8cfd3eeb3259d0`
  (execution code matches `origin/main` at
  `a1278be5fdff24d109d9e03134c6bdb880577f64`; the intervening change is
  documentation only)
* Parser/query data was preinstalled outside the measured interval.

For each steady-state scenario, direct and relay order alternated across 10
independent process pairs. Cache-hit runs issued 1,000 requests per process;
edit runs issued 100 requests per process. Reported confidence intervals are a
deterministic paired bootstrap over the 10 run-level summaries. Percentiles are
rounded to 0.1 ms by the driver, so sub-0.1-ms percentile differences are below
its reporting resolution.

## Results

### Steady-state request latency

| Scenario | Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|---:|
| Rust small, unchanged cache hit | p50 | 0.30 ms | 0.30 ms | 0.00 ms | [0.00, 0.00] ms |
| Rust small, unchanged cache hit | p95 | 0.53 ms | 0.53 ms | 0.00 ms | [-0.03, 0.03] ms |
| Rust small, unchanged cache hit | p99 | 0.87 ms | 0.94 ms | +0.07 ms | [-0.01, 0.16] ms |
| Rust small, one edit/request | p50 | 3.67 ms | 3.73 ms | +0.06 ms | [0.01, 0.12] ms |
| Rust small, one edit/request | p95 | 4.33 ms | 4.42 ms | +0.09 ms | [-0.10, 0.29] ms |
| Rust small, one edit/request | p99 | 5.08 ms | 5.29 ms | +0.21 ms | [-0.28, 0.82] ms |
| Markdown injections, one edit/request | p50 | 2.54 ms | 2.53 ms | -0.01 ms | [-0.03, 0.00] ms |
| Markdown injections, one edit/request | p95 | 2.83 ms | 2.72 ms | -0.11 ms | [-0.26, 0.00] ms |
| Markdown injections, one edit/request | p99 | 5.04 ms | 5.45 ms | +0.41 ms | [-0.26, 1.09] ms |

The negative Markdown deltas are not interpreted as an improvement: the relay
does no useful work, the intervals are dominated by run-level scheduling noise,
and one direct run had an elevated p95.

### Throughput-sensitive cache-hit path

The cache-hit path transferred approximately 14.3 MiB of response bodies per
1,000-request run.

| Metric | Direct mean | Relay mean | Paired delta | 95% CI for delta |
|---|---:|---:|---:|---:|
| Wall time / 1,000 requests | 360.5 ms | 371.4 ms | +10.9 ms (+3.0%) | [6.1, 16.4] ms |
| Amortized extra wall time | — | — | +10.9 µs/request | [6.1, 16.4] µs/request |

This is the clearest measured lower bound for the resident transport hop. The
edit scenarios include a fixed 10-ms delay after each edit, so their total wall
times intentionally cannot isolate a similarly small transport cost.

### Fresh-process path

`hyperfine` ran 20 fresh processes per condition, after three warmups, with one
immediate Rust request:

| Path | Mean | Standard deviation | Range |
|---|---:|---:|---:|
| Direct | 96.6 ms | 6.6 ms | 87.1–108.0 ms |
| Relay | 115.0 ms | 6.0 ms | 106.6–124.7 ms |

The observed +18.4 ms includes starting a Python interpreter and is not an
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

The additional raw process/pipe hop is not a steady-state performance blocker
on this machine. Its median and p95 costs were at or below the driver's 0.1-ms
reporting resolution, while the most sensitive cache-hit throughput run showed
about 10.9 microseconds of amortized extra wall time per request.

This result is necessary but insufficient evidence for the ADR. It does not
show a worker performance improvement, and it cannot detect costs or benefits
from:

* encoding and decoding the actual versioned worker protocol;
* maintaining duplicated authoritative document text;
* worker queue wait and parent resume scheduling;
* fused tree derivation, worker-local caches, or removal of parent-side locks;
* multi-document fairness, obsolete-work cancellation, or internal threading;
* worker memory, cold start, crash recovery, and full resynchronization; or
* hazard and native-segment control handshakes.

The result supports proceeding to ADR Stage 1 because the unavoidable raw
transport lower bound is small enough that higher-level architectural gains
could dominate it. The ADR must remain `proposed` until the real
`DeriveSnapshot` prototype measures the complete gate matrix.

## Reproduction

The tail-percentile driver and relay are in `benches/profile/drive.py` and
`benches/profile/worker_proxy.py`. A representative pair is:

```sh
python3 benches/profile/drive.py \
  --bin ./target/release/kakehashi \
  --data-dir ./deps/test/kakehashi \
  --lang rust --size 15 --requests 100 --edits 1

KAKEHASHI_WORKER_PROXY_BIN=./target/release/kakehashi \
python3 benches/profile/drive.py \
  --bin ./benches/profile/worker_proxy.py \
  --data-dir ./deps/test/kakehashi \
  --lang rust --size 15 --requests 100 --edits 1
```
