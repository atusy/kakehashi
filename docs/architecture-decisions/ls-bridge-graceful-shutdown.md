# LS Bridge Graceful Shutdown

## Context

ls-bridge-async-connection (Async Bridge Connection), ls-bridge-message-ordering (Message Ordering), and ls-bridge-server-pool-coordination (Server Pool Coordination) establish the communication architecture but do not specify shutdown behavior.

### Critical Gaps Without Shutdown Specification

1. **No LSP shutdown handshake**: LSP protocol requires `shutdown` request → `exit` notification sequence for clean server termination
2. **Undefined operation disposal**: What happens to pending operations and queued requests during shutdown?
3. **No state for shutdown-in-progress**: ConnectionState (Initializing/Ready/Failed) has no "shutting down" state, creating race conditions
4. **Multi-connection coordination unspecified**: How to shut down multiple concurrent language servers (parallel vs sequential, timeout handling)

**Without Graceful Shutdown:**
- Servers may not flush buffers or save state
- Operations hang indefinitely or receive no error response
- Process cleanup may leak resources (zombie processes, lock files)
- LSP protocol violations may corrupt server caches

## Decision

**Implement two-tier graceful shutdown with LSP protocol compliance and fail-fast operation disposal.**

## Architecture

### Connection State for Shutdown

This decision defines behavior for `Closing` and `Closed` states. See ls-bridge-message-ordering § Connection State Tracking for the complete ConnectionState enum.

**Shutdown-Specific Transitions:**
```
Ready → Closing          (graceful shutdown initiated)
Initializing → Closing   (abort initialization, shutdown)
Closing → Closed         (shutdown completed or timed out)
Failed → Closed          (skip shutdown handshake, cleanup only)
```

**Operation Gating in Closing State:**
- New operations: Reject with `REQUEST_FAILED` ("bridge: connection closing")
- Order queue: Continue draining (send queued operations)
- Pending responses: Wait up to the applicable shutdown deadline (per-slot
  `stop` or global teardown), failing at connection closure if it comes
  first

### LSP Shutdown Handshake Sequence

Follow LSP specification's two-phase shutdown:

```
┌─────────────────────────────────────────────────────────┐
│ Phase 1: Graceful Shutdown                              │
│ ────────────────────────────────────────────────────    │
│ 1. Transition to Closing state                          │
│ 2. Send LSP shutdown request to server                  │
│ 3. Wait for shutdown response (until the deadline)      │
│ 4. Send LSP exit notification                           │
│ 5. Wait for process exit (until the deadline)           │
│ 6. Transition to Closed state                           │
└─────────────────────────────────────────────────────────┘
                           │
                           │ Timeout expires
                           ▼
┌─────────────────────────────────────────────────────────┐
│ Phase 2: Forced Shutdown                                │
│ ────────────────────────────────────────────────────────│
│ 1. Send SIGTERM to server process                       │
│ 2. Wait for process death (implementation-defined)      │
│ 3. Send SIGKILL if still alive                          │
│ 4. Transition to Closed state                           │
└─────────────────────────────────────────────────────────┘
```

**Exception: Failed State Bypass**
```
Failed → Closed (skip LSP handshake)
├─ stdin unavailable (writer loop panicked or process crashed)
├─ Send SIGTERM immediately
└─ Wait for process exit, then SIGKILL if needed
```

**Unconfirmed termination at the global deadline**: the connection still
transitions to `Closed` for bookkeeping. What happens to the child
depends on whether the kakehashi process is actually exiting:
log-and-abandon applies **only on the process-exit path** (the `exit`
notification, or teardown that ends the process). When `shutdown_all`
runs while the server stays alive — the LSP `shutdown` request is
answered and the process then waits for `exit`, possibly indefinitely —
ownership of every unconfirmed child is **retained**: its
termination-pending record and background `wait` persist past the
teardown, so a client that delays or omits `exit` cannot leave a live
child unowned or unreaped. This bookkeeping-only `Closed` is the one
exception to bridge-client-control-protocol's steady-state rule that an
unconfirmed termination never reaches `Closed`.

### Operation Disposal Policy: Reject New, Drain Accepted, Fail Pending at Closure or Deadline

**Decision**: Reject new operations immediately, drain the already-accepted
order queue ahead of the shutdown handshake, and fail pending responses at
connection closure or the applicable deadline, whichever comes first.

**Rationale:**
- **Predictable latency**: Bounded shutdown time, no waiting for slow servers
- **Clear error semantics**: Operations receive explicit failure, not timeout
- **Stream integrity**: The accepted queue drains in FIFO order — the
  writer's existing behavior — so `shutdown` cannot overtake messages the
  bridge already committed to send

**Operation Handling:**

| Operation Location | Shutdown Action |
|-------------------|-----------------|
| **Order queue - Not yet dequeued** | Drained: sent before `shutdown`, preserving FIFO order |
| **Order queue - Currently writing** | Complete write (never abort mid-stream) |
| **Pending responses** | Fail with `REQUEST_FAILED` ("bridge: connection closing") at connection closure or when the timeout expires, whichever comes first |
| **New operations** | Reject with `REQUEST_FAILED` ("bridge: connection closing") when attempting to enqueue |

**Why not abort mid-write**: Operations in the order queue may be partially written to stdin. Aborting mid-write corrupts the protocol stream.

**Forced-termination exception**: the drain and complete-write rows describe
the graceful path. A forced termination (writer-idle timeout, deadline
expiry) kills the process regardless of queue state — the remaining queue is
abandoned, and its tracked requests fail exactly like pending responses.

### Writer Loop Shutdown Synchronization

**Problem**: Writer loop and shutdown sequence both write to stdin. Concurrent writes corrupt protocol stream.

**Solution**: Three-phase shutdown coordination.

**Phase 1: Signal Stop**
```rust
// Shutdown sequence
async fn graceful_shutdown(&self) {
    // 1. Transition to Closing state (new operations rejected)
    self.state.set(Closing);

    // 2. Atomically close queue admission: the close() is the seal —
    //    after it, no enqueue can succeed, even from a caller holding a
    //    stale Ready view of the handle. Everything accepted before the
    //    close is still in the queue and will be drained.
    self.order_queue.close();

    // Phase 2: Wait for writer to become idle...
}

// Writer loop — owns its state by value (incl. stdin), matching the
// actual writer task; ownership is what makes the handoff transfer real
async fn writer_loop(mut self) {
    // recv() on a closed queue yields the remaining accepted items in
    // FIFO order, then None — so the loop drains everything accepted
    // before the seal and only then exits (§ Operation Disposal Policy).
    while let Some(operation) = self.order_queue.recv().await {
        // Write operation... (never aborted mid-write)
    }

    // Hand stdin back: idle signal AND ownership transfer in one send
    let _ = self.writer_idle_tx.send(self.stdin);
}
```

**Phase 2: Wait for Idle**
```rust
// Shutdown sequence continues
async fn graceful_shutdown(&self) {
    // Wait for the writer to hand stdin back (or timeout). Idle alone
    // is not enough: Phase 3 needs the returned stdin handle for
    // exclusive access, and a CLOSED channel means the writer died
    // without handing off — treat it exactly like a timeout.
    let stdin = match tokio::time::timeout(
        Duration::from_secs(2),
        self.writer_idle_rx.recv()
    ).await {
        Ok(Some(stdin)) => stdin, // handoff complete
        Ok(None) | Err(_) => {
            // The writer may still own stdin or sit mid-write; writing
            // the shutdown request now could interleave with a partial
            // frame. Skip the LSP handshake entirely and force
            // terminate (SIGTERM → SIGKILL) — exclusive access was
            // never acquired, so no LSP goodbye is possible.
            log::warn!("Writer loop timeout; skipping LSP handshake");
            return self.force_terminate().await;
        }
    };

    // Phase 3: Exclusive stdin access via the handed-back `stdin`...
}
```

**Phase 3: Exclusive Access**
```rust
// Shutdown sequence continues
async fn graceful_shutdown(&self) {
    // NOW safe: `stdin` is the handle Phase 2 received from the writer —
    // ownership, not just quiescence
    self.send_shutdown_request(&mut stdin).await?;

    // Wait for shutdown response...
    // Send exit notification...
    // Kill process...
}
```

**Guarantees (graceful path — a forced termination forfeits the queue
drain and current-write completion; the bounded wait is what enforces
it):**
- ✅ Writer loop stops dequeuing **before** shutdown writes to stdin
- ✅ No concurrent writes to stdin (sequential: writer → shutdown)
- ✅ Bounded wait (2s timeout prevents indefinite hang)
- ✅ Current write completes (no mid-write abortion)

**Why 2-second timeout**: Writer loop writes typically <100ms. 2s allows for slow disk I/O without indefinite hang.

**Note**: This 2s timeout is per-connection and runs inside `graceful_shutdown()`, which itself runs under the applicable shutdown deadline (per-slot `stop` or global teardown). The 2s counts against that budget.

### Shutdown Timeout Policy

**Global timeout**: Implementation-defined duration (typically 5-15 seconds) bounding the termination *attempt* (escalation) across all connections. Ownership disposition and local cleanup (registries, router state) are lock-bounded local work needing no server cooperation; they run immediately after and fall outside the ceiling.

**Best-Effort Parallel Shutdown:**
All connections shut down in parallel under a single global ceiling. This is intentionally best-effort:
- No per-connection budget allocation (avoids complexity)
- Fast servers complete quickly; slow servers use remaining time
- If global timeout expires, all remaining connections are force-killed
- No fairness guarantee: a very slow server may consume most of the budget

**Rationale for Global Timeout:**
- Multi-server coordination requires bounded total time
- User experience: Shutdown shouldn't hang indefinitely
- Per-server timeout could multiply (5 servers × 5s = 25s unacceptable)
- Fast servers don't wait for slow servers to time out
- Simplicity: No complex budget-splitting logic

**Timeout Application:**
```rust
enum ShutdownMode {
    ProcessExit,   // exit notification / signal path: kakehashi is ending
    ServerRemains, // LSP shutdown request: server stays alive awaiting exit
}
// Teardown is a SHARED single-flight operation: concurrent callers join
// the same run rather than returning early; the effective mode upgrades
// monotonically (ProcessExit dominates ServerRemains, even mid-run), and
// every caller joins completion before proceeding — an exit path racing
// an in-progress ServerRemains teardown cannot terminate around it.

async fn shutdown_all_connections(pool: &ConnectionPool, mode: ShutdownMode) {
    // Both deadlines are absolute and anchored at FUNCTION ENTRY, so
    // sealing/snapshot work spends the same ceiling it lives under.
    // The gap between them is the escalation reserve
    // (ls-bridge-timeout-hierarchy: ~20% for SIGTERM/SIGKILL).
    let deadline = Instant::now() + GLOBAL_TIMEOUT;
    let graceful_deadline = deadline - GLOBAL_TIMEOUT.mul_f32(0.2);

    // Gate ordinary admission: the pool-wide shutting_down flag stops
    // acquires from spawning new connections from here on.
    pool.mark_shutting_down();

    // Seal control admission before any snapshot: no new per-slot
    // stop/restart (bridge-client-control-protocol) may begin, ownership
    // of in-flight control operations transfers to this teardown, and
    // the seal blocks replacement insertion. Control operations register
    // every process they spawn in the teardown-owned `control_procs`
    // registry AT SPAWN TIME, so a restart crossing its spawn point can
    // never lose its kill handle. Adoption SIGNALS cooperative
    // cancellation immediately — the seal is the signal each task
    // observes at its next commit point — so the graceful phase below
    // doubles as the cooperative drain. `control_tasks` is a
    // teardown-owned JoinSet living OUTSIDE the timed future below, so
    // a timeout drops only the polling future, never the task handles;
    // both phases poll the same set. `control_procs` is a LIVE registry
    // with kill-on-register from this point on — a child spawned after
    // any scan is killed at registration — and it closes only when the
    // last control task terminates. It covers EVERY process a control
    // operation owns — a pre-existing server whose writer a stop
    // reclaimed as much as a fresh replacement. The seal also quiesces
    // the normal-service reaper, takes over its in-progress
    // finalizations as teardown-run settlement work OUTSIDE this
    // abortable set (claims revocable, settlement idempotent), and
    // TRANSFERS existing termination-pending records — kill handles and
    // pending Child::wait futures from earlier stopFailed slots — into
    // control_procs, so a child from a failed stop is inside the
    // escalation set even though it is in neither the connection
    // snapshot nor the task set.
    let (mut control_tasks, control_procs) = seal_and_take_control_operations();

    // Snapshot AFTER both gates: nothing can be inserted past them.
    let connections = pool.all_connections();

    let graceful = tokio::time::timeout_at(graceful_deadline, async {
        // Shutdown all connections in parallel (best-effort).
        // Each task uses the state-aware dispatch below (§ Multi-Connection
        // Shutdown): Failed → cleanup_only, Initializing →
        // terminate_without_lsp, otherwise graceful_shutdown.
        let tasks = connections.iter()
            .map(|conn| conn.shutdown_by_state());

        // Poll the teardown-owned JoinSet alongside; the set itself
        // outlives this future, so nothing detaches on timeout
        join!(futures::future::join_all(tasks),
              drain_join_set(&mut control_tasks));
    }).await;

    // Survivor handling runs CONCURRENTLY with escalation. PRODUCER
    // CUTOFF: cancellation was signalled at adoption and the graceful
    // phase was the cooperative drain, so EVERY task still alive at
    // graceful_deadline — wedged before, between, or after commit
    // points — is hard-aborted at the cutoff (not the final deadline),
    // closing the process registry; the escalation reserve always kills
    // a CLOSED set and a child registered near expiry cannot escape
    // the escalation set (the kill attempt is what is guaranteed;
    // confirmation remains the reaped wait). Hard-abort is safe by commit-point
    // atomicity (bridge-client-control-protocol). Escalation covers the
    // snapshot and every registered control process alike (no-op for
    // connections already Closed; a control-owned key contributing both
    // a Closing handle and its task is benign — shutdown_by_state on
    // Closing/Closed is a compare-transition no-op). When the remaining
    // budget is shorter than the normal SIGTERM grace, the grace is
    // shortened or skipped.
    // abort_survivors_and_finalize FINALIZES before returning: per
    // bridge-client-control-protocol it runs settlement for EVERY
    // non-normal terminal outcome (hard abort, cancellation short of
    // completion, panic — even one already reaped by the graceful
    // drain, via the durable operation record), settling ARM,
    // tombstone, replacement, registry state, and the outer result
    // channel to what the last recorded commit implies. During teardown
    // settlement touches RECORDS AND POOL STATE ONLY — process
    // termination belongs exclusively to the escalation arm via the
    // live registry (single owner; no double kill/reap) — and it is
    // synchronous lock-bounded work run by teardown itself, outside the
    // abortable set, so the expired producer cutoff costs it nothing
    // and revocable idempotent claims mean an interrupted finalizer
    // never strands a record.
    let (unresolved, _) = join!(
        force_kill_remaining_until(deadline, connections, &control_procs),
        abort_survivors_and_finalize(&mut control_tasks, graceful_deadline),
    );

    // Mode-dependent disposition of children still unconfirmed at the
    // deadline (§ Unconfirmed termination). Disposition is lock-bounded
    // local work — like cleanup, it runs right after the ceiling rather
    // than inside it. Records retained under ServerRemains are disposed
    // at actual process end by their Drop owner; no teardown re-entry
    // is promised or required.
    match mode {
        ShutdownMode::ProcessExit  => log_and_abandon(unresolved),
        ShutdownMode::ServerRemains =>
            pool.reinsert_termination_pending(unresolved), // atomic; waits restarted
    }

    // Only now — every control task terminated, finalized, and the live
    // process registry closed or handed back — may pool/router cleanup
    // run. The invariant is scoped to ADOPTED CONTROL TASKS: none of
    // them can mutate registries or tombstones after cleanup; the
    // termination-pending registry and its background waiter
    // deliberately survive it (ServerRemains) and keep converting
    // records to tombstones as waits complete.
}
```

### Initialization Shutdown: Abort Immediately, No LSP Message

**Decision**: Abort initialization and terminate directly, sending no LSP
message at all.

**Sequence:**
```
Connection state: Initializing
Shutdown signal arrives
├─ Transition: Initializing → Closing
├─ Fail pending initialization request (if sent)
├─ Kill process (SIGTERM → SIGKILL) — no shutdown request, no exit notification
└─ Transition: Closing → Closed
```

**Rationale:**
- Initialization may hang (slow server, network issue)
- Waiting for initialization during shutdown adds unbounded latency
- Until the server has responded to `initialize`, LSP forbids the client
  every additional request **and notification** — `exit` included — so the
  only conformant abort is process termination with no LSP message. (An
  earlier revision of this rule sent `exit` in this window; that was
  non-conformant and was corrected alongside
  bridge-client-control-protocol, whose per-slot `stop` shares this path.)

### Multi-Connection Shutdown: Parallel with Global Timeout

**Decision**: Shut down all connections in parallel with single global timeout.

**Coordination Strategy:**

```rust
async fn shutdown_router(mode: ShutdownMode) {
    // 0. One absolute ceiling, anchored before any teardown work begins;
    //    the escalation reserve is the gap to graceful_deadline
    let deadline = Instant::now() + GLOBAL_TIMEOUT;
    let graceful_deadline = deadline - GLOBAL_TIMEOUT.mul_f32(0.2);

    // 1. Stop accepting new requests (also gates ordinary acquires via
    //    the pool-wide shutting_down flag)
    mark_router_shutting_down();

    // 2. Fail all pending routing decisions
    fail_pending_routes();

    // 3. Seal control admission and adopt in-flight stop/restart
    //    operations (bridge-client-control-protocol): the seal blocks
    //    replacement insertion, so a registry-only restart can neither
    //    escape the connection snapshot below nor leave a live
    //    replacement behind; every process a control operation spawns is
    //    registered in the teardown-owned live control_procs registry at
    //    spawn time (kill-on-register once teardown begins). The
    //    registry covers EVERY process a control operation owns —
    //    reclaimed pre-existing servers included — and the seal also
    //    quiesces the normal-service reaper, takes over in-progress
    //    finalizations as teardown-run settlement work, and transfers
    //    existing termination-pending records; unresolved survivors
    //    hand back per the sketch above. The JoinSet outlives the timed
    //    future, as in the sketch above
    let (mut control_tasks, control_procs) = seal_and_take_control_operations();

    // 4. Shutdown all connections in parallel (snapshot AFTER both gates)
    let all_connections = connection_pool.all_connections();

    // Graceful phase bounded by the absolute graceful deadline
    let graceful = tokio::time::timeout_at(graceful_deadline, async {
        let tasks = all_connections.iter()
            .map(|conn| async move {
                // Atomic compare-transition: each arm commits its
                // *→Closing transition conditionally (Failed → cleanup
                // only, Initializing → terminate without LSP, Ready →
                // full handshake), so a handshake that commits Ready
                // between observe and act is shut down gracefully,
                // never terminated without the handshake.
                conn.shutdown_by_state().await
            });

        // Adopted control operations are polled from the outer JoinSet
        join!(futures::future::join_all(tasks),
              drain_join_set(&mut control_tasks));
    }).await;

    // Cancellation/join runs concurrently with escalation against the
    // same absolute deadline; a task still alive at the deadline is
    // hard-aborted (safe: shared-state effects live only at commit
    // points — see the sketch above and bridge-client-control-protocol)
    // Cancellation signalled at adoption; producer cutoff at
    // graceful_deadline; finalization for every non-normal outcome; the
    // caller passes the same ShutdownMode and the unresolved set takes
    // the same mode branch — see the sketch above
    let (unresolved, _) = join!(
        force_kill_remaining_until(deadline, all_connections, &control_procs),
        abort_survivors_and_finalize(&mut control_tasks, graceful_deadline),
    );
    dispose_unresolved(mode, unresolved);

    // 5. Clean up router resources — runs after every control task has
    //    terminated (the join above), so no ADOPTED CONTROL TASK can
    //    mutate registries or tombstones behind it; the
    //    termination-pending registry and its waiter survive cleanup
    //    under ServerRemains, as in the sketch above. Cleanup is local,
    //    lock-bounded work: the shutdown ceiling scopes the termination
    //    attempt, not this step, which needs no server cooperation.
    cleanup_router_state();
}
```

**Why Parallel:**
- **Bounded total time**: N servers shut down in O(1) time, not O(N)
- **Independent failures**: Hung server doesn't block others
- **User experience**: 3 servers × 5s sequential = 15s vs 5s parallel

## Consequences

### Positive

**LSP Protocol Compliance:**
- Servers receive proper shutdown request → exit notification sequence
- Allows servers to flush buffers, save state, release locks
- Prevents cache corruption from abrupt termination

**Bounded Shutdown Latency:**
- Global timeout bounds the termination *attempt* — escalation and
  ownership handling, not confirmed termination (local cleanup falls
  outside the ceiling and needs no server cooperation)
- Fail-fast disposal of pending and new operations prevents hang; on the
  graceful path the accepted write queue drains — the *amount* of work is
  queue-bounded, but each write can block on a full pipe if the
  downstream stops reading, so the drain stays deadline-bounded and may
  end in forced termination
- Parallel multi-connection shutdown: O(1) not O(N)

**Clear Error Semantics:**
- Operations in flight receive explicit errors, not timeout
- New operations rejected immediately during shutdown (Closing state)
- Users see "bridge: connection closing" error, not silent hang

**Resource Cleanup:**
- SIGTERM → SIGKILL sequence terminates processes in the ordinary case;
  a child unconfirmed at the deadline is retained via its
  termination-pending record (or logged and abandoned on the
  process-exit path — see § Unconfirmed termination)
- No zombie processes or leaked file descriptors while kakehashi lives
  (pending `wait`s are driven to completion)
- Lock files and caches properly released

**Multi-Server Resilience:**
- Hung server doesn't block shutdown of healthy servers
- Failed connections use fast path (skip LSP handshake)
- Global timeout prevents indefinite hang

### Negative

**No Response Draining:**
- On the graceful path accepted writes drain to the server, but their
  responses are not awaited — pending operations fail at connection
  closure or the deadline, whichever comes first (forced termination
  abandons the queue as well)
- May surprise users expecting "finish pending work"
- Trade-off: Predictable shutdown time vs completion

**Failed Connections Bypass LSP:**
- Servers with Failed state don't receive shutdown request
- May leave caches in inconsistent state
- Mitigation: Servers should handle abrupt termination (crash recovery)

**Global Timeout Pressure:**
- Fast servers must wait for slow servers (up to timeout)
- Very slow servers force-killed even if making progress
- Alternative (per-server timeout) has worse UX (unbounded total time)

**Initialization Abort Abrupt:**
- Servers in Initializing state killed without completing setup
- May leave partial initialization state
- Trade-off: Shutdown latency vs initialization completion

### Neutral

**Implementation-Defined Timeout:**
- Flexibility for different deployment scenarios
- Must be documented/configurable for operators

**Closing State Overhead:**
- Adds complexity to state machine
- Necessary to prevent shutdown race conditions

## Alternatives Considered

### Alternative 1: Sequential Multi-Connection Shutdown

Shut down connections one at a time with individual timeouts.

**Rejected Reasons:**

1. **Unbounded total time**: N servers × timeout = potentially very long wait (3 servers × 5s = 15s)
2. **Poor user experience**: User waits for each server sequentially
3. **Slow server blocks all**: First server hangs → all others wait
4. **No benefit over parallel**: Independent connections can shut down concurrently

**Why parallel is better**: Bounded total time (global timeout), better UX, fault isolation.

### Alternative 2: Drain Operations Before Shutdown

Continue processing pending operations until complete before shutting down.

**Scope note**: what is rejected here is waiting for operation
*completion* — server responses. Draining the already-accepted write queue
is retained (§ Operation Disposal Policy): its work is FIFO-bounded and
distinct from waiting for responses, though the write side is still
bounded by the applicable deadline — a full pipe can stall it into forced
termination.

**Rejected Reasons:**

1. **Unbounded shutdown time**: Slow operations could delay shutdown indefinitely
2. **Complexity**: Must track partial completion, handle new operations during drain
3. **LSP violation risk**: New operations arriving while draining create race conditions
4. **User expectation mismatch**: Users expect shutdown to be fast, not "finish all work first"

**Why fail-fast is better**: Predictable latency, simpler implementation, clear error semantics.

### Alternative 3: No Writer Loop Synchronization

Skip synchronization, just send shutdown request whenever ready.

**Rejected Reasons:**

1. **Protocol stream corruption**: Concurrent writes to stdin cause byte-level interleaving
2. **LSP violation**: Corrupted JSON-RPC stream causes parse errors
3. **Hard to debug**: Intermittent failures due to race conditions
4. **No recovery**: Once stream corrupted, connection unusable

**Why synchronization is essential**: Protocol correctness requires serialized stdin writes.

## Related Decisions

- **[ls-bridge-async-connection](ls-bridge-async-connection.md)**: Async Bridge Connection
  - Uses shutdown signal from `select!` pattern
  - ls-bridge-graceful-shutdown adds LSP handshake and process cleanup
- **[ls-bridge-message-ordering](ls-bridge-message-ordering.md)**: Message Ordering
  - Extends ConnectionState enum with Closing/Closed states
  - Defines operation disposal for pending requests
- **[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)**: Server Pool Coordination
  - ls-bridge-graceful-shutdown defines router shutdown coordination strategy
  - Parallel shutdown with global timeout
- **[ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md)**: Timeout Hierarchy
  - Global shutdown timeout takes precedence over other timeouts
  - Liveness timeout disabled during Closing state

## References

**LSP Specification**: [Shutdown Request](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#shutdown)
- Servers must receive `shutdown` request before `exit` notification
- Servers use shutdown phase to flush buffers and save state

**Process Management**: SIGTERM → SIGKILL pattern
- SIGTERM allows graceful cleanup
- SIGKILL is the last resort; delivery does not prove exit — confirmation
  is the reaped `wait`

## Amendment History

- **2026-01-06**: Merged Amendment 001 - Added three-phase writer loop shutdown synchronization to prevent stdin corruption during concurrent shutdown writes
- **2026-08-11**: Corrected Initialization Shutdown - the abort path sends no LSP message at all (the earlier revision sent `exit` before the initialize response, which LSP ordering forbids); adopted alongside bridge-client-control-protocol, whose per-slot `stop` shares the path
- **2026-08-11**: Reconciled the Operation Disposal Policy with the Closing-state gating and the writer's actual behavior - the accepted order queue drains ahead of `shutdown` (the earlier table said queued operations are never sent, contradicting § Operation Gating and the FIFO writer)
