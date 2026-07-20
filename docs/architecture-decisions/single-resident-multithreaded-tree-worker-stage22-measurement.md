# Single Tree Worker Stage 22 Linux Parent-Death Measurement

## Scope

Stage 22 layers Linux `PR_SET_PDEATHSIG(SIGKILL)` over the portable Stage 21
liveness pipe. The worker validates one immutable startup contract, registers
the signal before observable work, verifies it with `PR_GET_PDEATHSIG`, and
immediately compares the actual and expected parent PIDs to close the kernel
registration race.

This hardening kills the direct worker when its spawning supervisor thread
dies. It does not claim abnormal-parent descendant cleanup; the liveness pipe
and explicit Unix process-group cleanup remain separate mechanisms.

## Correctness result

The Linux-only process topology deliberately keeps both worker stdin and the
Stage 21 liveness-pipe writer open. It therefore proves kernel behavior rather
than accidentally passing through EOF. One test observes the correctly armed
`SIGKILL` registration and then real parent death. A second pauses before
registration, kills the parent, releases the worker, and proves that the
post-registration PID comparison exits before the worker becomes ready.

The initial RED run failed both assertions for the intended missing behavior.
The GREEN run passed both tests on Ubuntu. Default checks, Clippy, rustfmt,
audit, the full test job, and all-target/all-feature checks on Ubuntu, macOS,
and Windows also passed.

## Performance result

GitHub Actions run `29745870463` built immutable Stage 21 and Stage 22 release
binaries and their matching benchmark harnesses in independent target
directories on Ubuntu 24.04 (`AMD EPYC 9V74`). Matching harness generations is
necessary because Stage 22 adds the private `--expected-parent-pid` worker
argument; cross-generation CLI compatibility is a later protocol gate, not a
valid startup comparison.

After two warmup pairs, twelve measured pairs alternated baseline-candidate and
candidate-baseline order. Each worker used four threads. Direct startup measures
`Client::spawn` through `HandshakeReady`; end-to-end startup uses a fresh XDG
environment and validates one small Rust semantic response before clean
shutdown.

| Metric | Stage 21 median | Stage 22 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Worker handshake | 80.002 ms | 80.024 ms | -0.028% |
| LSP process through clean shutdown | 273.915 ms | 274.118 ms | +0.430% |
| First semantic request | 101.556 ms | 101.737 ms | +0.284% |

Handshake deltas split evenly: Stage 22 was faster in six pairs and slower in
six. One Stage 22 handshake was a 95.152 ms outlier, but the median remained
stable. The sub-percent end-to-end differences are small relative to observed
runner variation, so this experiment detects neither a regression nor an
improvement. Raw values and binary digests are in
`benches/profile/results/single_worker_stage22_linux_pdeathsig_2026-07-20.json`.

## Decision

Keep Stage 22. Linux parent-death hardening closes a correctness gap without a
detected startup cost. Keep the ADR `proposed`: Windows ownership, native
hazard/quarantine control, protocol and compatibility gates, and legacy-path
removal remain open.
