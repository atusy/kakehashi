# Single Tree Worker Stage 23 Windows Supervision Measurement

## Scope

Stage 23 makes the Windows LSP parent own a private Job Object configured with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The worker remains behind a bootstrap gate
until the parent assigns that Job and duplicates a validated parent-process
handle into the child. A dedicated worker control thread waits on that handle
and uses hard process termination, so shutdown does not depend on a native
compute thread unwinding.

## Correctness result

The first Windows run was not valid RED evidence because an unrelated parser
fixture path failed before the intended hang. The corrected RED run
`29750012782` avoided parser loading and reached the process-lifecycle assertion:
the direct worker exited on transport EOF, but its descendant survived parent
death.

The GREEN implementation passed three independent Windows cases in run
`29751887031`: parent-handle monitoring alone terminates a worker whose compute
completion never arrives; the Job alone terminates the worker and its real
descendant; and the production combination terminates both. Tests acquire real
process handles before killing the LSP parent, avoiding PID-reuse races. The
same run passed cross-platform compilation on Windows, macOS, and Ubuntu.

## Performance result

Run `29751887031` built immutable Stage 22 and Stage 23 release binaries and the
same parser-free startup harness against each generation on Windows Server 2025
(`AMD64 Family 25 Model 1 Stepping 1, AuthenticAMD`). This isolates Job creation,
handle duplication, Job assignment, bootstrap framing, and monitor-thread
startup from parser loading.

After five warmup pairs, thirty measured pairs alternated
baseline-candidate/candidate-baseline order. Each worker used four compute
threads.

| Metric | Stage 22 median | Stage 23 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Worker handshake | 73.649 ms | 74.023 ms | +0.537% |
| Clean shutdown | 5.283 ms | 5.298 ms | +0.171% |

The median absolute handshake increase was 0.396 ms, with Stage 23 slower in 22
of 30 pairs. This is a small but repeatable startup cost, not a performance
improvement. Shutdown split nearly evenly (Stage 23 faster in 14 pairs and
slower in 16), so the experiment detects no meaningful shutdown change. Raw
values and harness digests are in
`benches/profile/results/single_worker_stage23_windows_supervision_2026-07-20.json`.

## Decision

Keep Stage 23. Roughly four-tenths of a millisecond at one resident worker
startup is an acceptable fixed cost for deterministic abnormal-parent and
descendant cleanup. It does not affect steady-state parse throughput. Keep the
ADR `proposed`: native crash containment/quarantine, compatibility gates, and
legacy-path removal remain open.
