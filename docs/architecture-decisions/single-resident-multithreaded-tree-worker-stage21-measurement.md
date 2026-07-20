# Single Tree Worker Stage 21 Parent-Liveness Measurement

## Scope

Stage 21 gives the Unix worker a parent-liveness channel independent of its
compute pool and protocol reader. The parent owns a close-on-exec pipe writer;
only the worker inherits the reader. A dedicated control thread blocks on that
reader and calls `_exit` on EOF, so parent death cannot wait for a hung native
compute thread or Rust destructors.

This is the ADR's portable Unix baseline. Linux `PR_SET_PDEATHSIG` and its PID
race check, plus Windows parent-handle and Job Object ownership, remain open.

## Method and result

The RED process E2E parked one worker compute thread forever, killed the real
LSP parent, and observed that the worker remained orphaned. With the liveness
pipe, the same worker exited within the two-second assertion window. Three
measurement runs passed; full-test elapsed median was 3.923 seconds (range
3.892–16.977 seconds). The first run included incremental test build work.

Twelve order-reversed cold-start pairs compared default release Stage 20 and
Stage 21 binaries in authoritative mode with four worker threads, fresh config
and state directories, and one small Rust semantic request. Source commit was
`8e6549081d7609fc7e7eead0e97f4e3871f5587f`; binary SHA-256 values were
`ac0b197a425cb5812270cc89d9e108448d83f09b7ede7973b8b18658882a1761` and
`d1b321f0f1ef452627439110a4413647cd765f43c99014149c178139cdb6b25c`.

| Metric | Stage 20 median | Stage 21 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Driver elapsed | 2,352.94 ms | 2,348.87 ms | +0.15% |
| First request | 2,037.75 ms | 2,037.05 ms | +0.05% |

The first pair was an approximately one-second baseline outlier. Median paired
deltas are noise-scale; no startup regression or improvement is claimed. Raw
values are in
`benches/profile/results/single_worker_stage21_parent_liveness_2026-07-20.json`.

Validation passed for rustfmt, both process-group unit tests, the repeated
parent-death E2E, and Clippy across all targets and features.

## Decision

Keep Stage 21. It closes abnormal-parent-death containment on macOS and the
portable Unix path without touching request execution. Keep the ADR `proposed`
until Linux hardening, Windows ownership, native hazard/quarantine control,
protocol and compatibility gates, and legacy-path removal are complete.
