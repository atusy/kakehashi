# Single Tree Worker Stage 20 Descendant-Cleanup Measurement

## Scope

Stage 20 makes explicit worker cleanup descendant-safe on Unix. The parent
spawns the resident worker as its own process-group leader, retains the owned
group identity beside the direct `Child`, and targets that group from failed
handshake, request timeout, transport failure, drop, and graceful shutdown
paths. The parent still reaps the direct worker with a bounded wait; terminated
descendants are reparented and reaped by the operating system.

This stage does not claim the ADR's abnormal-parent-death contract. The
close-on-exec liveness pipe, Linux parent-death signal, hung-native-thread
fixture, and Windows Job Object remain separate gates.

## Method

### Lifecycle proof

A Unix unit fixture spawned a worker-like process-group leader with a live
descendant. Before the implementation, killing only the direct child left the
descendant alive and the RED test failed. The GREEN implementation terminates
the owned group and still reaps the direct child.

A process E2E then started the real LSP parent, resident worker, and an E2E-only
five-minute descendant. Normal LSP shutdown had to remove that descendant
within two seconds; natural fixture expiry cannot satisfy the assertion. The
complete scenario was repeated three times.

### Default release A/B

The Stage 19 and Stage 20 default-feature release binaries ran in authoritative
mode with four worker compute threads. The source commit was
`38840cd6d561b3cf99dfc5bb5fd85fb787867649`. Baseline SHA-256 was
`72dda444029ebdb9122e71401e36b960dca67b3165ebf5c3649480d5c2e35468`;
candidate SHA-256 was
`ac0b197a425cb5812270cc89d9e108448d83f09b7ede7973b8b18658882a1761`.

Twelve order-reversed cold-start runs per binary used fresh config and state
directories, one small Rust document, no settle delay, and one semantic
request. This targets the only ordinary-path change: assigning the worker's
Unix process group during spawn. Request execution itself is unchanged. Raw
values are in
`benches/profile/results/single_worker_stage20_descendant_cleanup_2026-07-20.json`.

## Result

The descendant-cleanup E2E passed 3/3 runs. Full-test elapsed median was
4.079 seconds, with a 3.937–4.240 second range.

Cold-start results were:

| Metric | Stage 19 median | Stage 20 median | Median paired delta |
| --- | ---: | ---: | ---: |
| Driver elapsed | 2,363.22 ms | 2,349.71 ms | -0.4% |
| First request | 2,036.65 ms | 2,028.50 ms | -0.6% |
| Wall time per cycle | 2,037.25 ms | 2,029.14 ms | -0.6% |

Stage 20 was faster in 8 of 12 driver-elapsed pairs and 7 of 12 first-request
pairs. Paired driver differences ranged from -128.86 to +61.57 milliseconds;
paired first-request differences ranged from -989.6 to +65.6 milliseconds.
The large first pair is an outlier in both process elapsed and request timing,
so the median is the gate and no startup improvement is claimed.

Validation passed:

* `cargo fmt --all -- --check`;
* `cargo test --lib` (3,148 passed, 2 ignored);
* the focused process-group unit tests;
* the real descendant-cleanup E2E in every focused and measurement run;
* `cargo test --features e2e --test tree_worker_process` (9 passed); and
* `cargo clippy --all-targets --all-features -- -D warnings`.

## Findings

### Ownership must include the cleanup target

Deriving a process-group ID from an arbitrary child at kill time couples
cleanup correctness to an unstated spawn assumption and risks targeting the
wrong group. `OwnedChild` establishes the group during spawn and retains that
identity through every termination path.

### Graceful worker exit does not prove descendant exit

Waiting for the direct worker is necessary for reaping but insufficient for
cleanup. Stage 20 explicitly targets the owned group even after the worker has
already exited successfully. The five-minute fixture exposed and prevents the
false-positive test that a short-lived descendant would create.

### Process groups solve explicit Unix cleanup, not parent death

If the parent is killed, its cleanup code cannot send the group signal. The
worker still needs an independent liveness thread and platform primitive. That
work remains visible rather than being implied by this stage.

## Decision

Keep Stage 20. It closes explicit Unix descendant cleanup without adding state,
branches, allocation, or synchronization to the compute/request path. Keep the
ADR `proposed` until abnormal parent-death handling, Windows ownership, native
hazard/quarantine control, protocol and compatibility gates, and legacy-path
removal are complete.
