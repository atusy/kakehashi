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
- Pending responses: Wait for responses up to global timeout

### LSP Shutdown Handshake Sequence

Follow LSP specification's two-phase shutdown:

```
┌─────────────────────────────────────────────────────────┐
│ Phase 1: Graceful Shutdown                              │
│ ────────────────────────────────────────────────────    │
│ 1. Transition to Closing state                          │
│ 2. Send LSP shutdown request to server                  │
│ 3. Wait for shutdown response (until global timeout)    │
│ 4. Send LSP exit notification                           │
│ 5. Wait for process exit (until global timeout)         │
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

### Operation Disposal Policy: Reject New, Drain Accepted, Fail Pending at Timeout

**Decision**: Reject new operations immediately, drain the already-accepted
order queue ahead of the shutdown handshake, and fail pending responses when
the timeout expires.

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

// Writer loop
async fn writer_loop(&self) {
    // recv() on a closed queue yields the remaining accepted items in
    // FIFO order, then None — so the loop drains everything accepted
    // before the seal and only then exits (§ Operation Disposal Policy).
    while let Some(operation) = self.order_queue.recv().await {
        // Write operation... (never aborted mid-write)
    }

    // Signal: writer is idle (queue fully drained)
    let _ = self.writer_idle_tx.send(());
}
```

**Phase 2: Wait for Idle**
```rust
// Shutdown sequence continues
async fn graceful_shutdown(&self) {
    // Wait for writer idle (or timeout)
    match tokio::time::timeout(
        Duration::from_secs(2),
        self.writer_idle_rx.recv()
    ).await {
        Ok(_) => log::debug!("Writer loop idle"),
        Err(_) => {
            // The writer may still own stdin or sit mid-write; writing
            // the shutdown request now could interleave with a partial
            // frame. Skip the LSP handshake entirely and force
            // terminate (SIGTERM → SIGKILL) — exclusive access was
            // never acquired, so no LSP goodbye is possible.
            log::warn!("Writer loop timeout; skipping LSP handshake");
            return self.force_terminate().await;
        }
    }

    // Phase 3: Exclusive stdin access...
}
```

**Phase 3: Exclusive Access**
```rust
// Shutdown sequence continues
async fn graceful_shutdown(&self) {
    // NOW safe to write to stdin (writer loop stopped)
    self.send_shutdown_request().await?;

    // Wait for shutdown response...
    // Send exit notification...
    // Kill process...
}
```

**Guarantees:**
- ✅ Writer loop stops dequeuing **before** shutdown writes to stdin
- ✅ No concurrent writes to stdin (sequential: writer → shutdown)
- ✅ Bounded wait (2s timeout prevents indefinite hang)
- ✅ Current write completes (no mid-write abortion)

**Why 2-second timeout**: Writer loop writes typically <100ms. 2s allows for slow disk I/O without indefinite hang.

**Note**: This 2s timeout is per-connection and runs inside `graceful_shutdown()`, which itself runs under the global shutdown timeout. The 2s counts against the global budget.

### Shutdown Timeout Policy

**Global timeout**: Implementation-defined duration (typically 5-15 seconds) for entire shutdown sequence across all connections.

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
async fn shutdown_all_connections(pool: &ConnectionPool) {
    let global_timeout = Duration::from_secs(IMPL_DEFINED);
    let deadline = Instant::now() + global_timeout;   // ONE absolute ceiling
    // Reserve part of the ceiling for forced escalation
    // (ls-bridge-timeout-hierarchy: ~20% for SIGTERM/SIGKILL)
    let graceful_budget = global_timeout.mul_f32(0.8);

    // Seal control admission FIRST, before any snapshot: no new per-slot
    // stop/restart (bridge-client-control-protocol) may begin, ownership
    // of in-flight control operations transfers to this teardown, and
    // the seal blocks replacement insertion — an adopted restart aborts
    // at its next commit point. Control operations register every
    // process they spawn in the teardown-owned `control_procs` registry
    // AT SPAWN TIME, so a restart crossing its spawn point can never
    // lose its kill handle.
    let (control_ops, control_procs) = seal_and_take_control_operations();

    // Snapshot AFTER the seal: nothing can be inserted past it.
    let connections = pool.all_connections();

    let graceful = tokio::time::timeout(graceful_budget, async {
        // Shutdown all connections in parallel (best-effort).
        // Each task uses the state-aware dispatch below (§ Multi-Connection
        // Shutdown): Failed → cleanup_only, Initializing →
        // terminate_without_lsp, otherwise graceful_shutdown.
        let tasks = connections.iter()
            .map(|conn| conn.shutdown_by_state());

        futures::future::join_all(tasks.chain(control_ops)).await;
    }).await;

    if graceful.is_err() {
        // Abort adopted control tasks and JOIN them before killing, so
        // none can mutate registries or tombstones after cleanup.
        abort_and_join_control_tasks().await;
    }

    // Escalation runs against the SAME absolute deadline, covering the
    // snapshot and every registered control process alike (no-op for
    // connections already Closed). When the remaining budget is shorter
    // than the normal SIGTERM grace, the grace is shortened or skipped —
    // the ceiling wins over politeness.
    force_kill_remaining_until(deadline, connections, control_procs);
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
async fn shutdown_router() {
    // 1. Stop accepting new requests
    mark_router_shutting_down();

    // 2. Fail all pending routing decisions
    fail_pending_routes();

    // 3. Seal control admission and adopt in-flight stop/restart
    //    operations (bridge-client-control-protocol): the seal blocks
    //    replacement insertion, so a registry-only restart can neither
    //    escape the connection snapshot below nor leave a live
    //    replacement behind; every process a control operation spawns is
    //    registered in the teardown-owned control_procs registry at
    //    spawn time
    let (control_ops, control_procs) = seal_and_take_control_operations();
    let deadline = Instant::now() + GLOBAL_TIMEOUT;

    // 4. Shutdown all connections in parallel (snapshot AFTER the seal)
    let all_connections = connection_pool.all_connections();

    // Reserve part of the single ceiling for forced escalation
    // (ls-bridge-timeout-hierarchy: ~20% for SIGTERM/SIGKILL)
    let graceful = tokio::time::timeout(GLOBAL_TIMEOUT.mul_f32(0.8), async {
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

        // Adopted control operations are joined alongside
        futures::future::join_all(tasks.chain(control_ops)).await;
    }).await;

    if graceful.is_err() {
        // Abort adopted control tasks and JOIN them before killing, so
        // none can mutate registries or tombstones after cleanup below
        abort_and_join_control_tasks().await;
    }

    // Escalation against the same absolute deadline; covers stragglers
    // and every registered control process alike (no-op for connections
    // already Closed). A remaining budget shorter than the normal
    // SIGTERM grace shortens or skips the grace — the ceiling wins.
    force_kill_remaining_until(deadline, all_connections, control_procs);

    // 5. Clean up router resources
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
- Global timeout ensures shutdown completes in bounded time
- Fail-fast disposal of pending and new operations prevents hang; the
  accepted write queue drains, but that is local FIFO work bounded by
  queue length, not by server speed
- Parallel multi-connection shutdown: O(1) not O(N)

**Clear Error Semantics:**
- Operations in flight receive explicit errors, not timeout
- New operations rejected immediately during shutdown (Closing state)
- Users see "bridge: connection closing" error, not silent hang

**Resource Cleanup:**
- SIGTERM → SIGKILL sequence ensures process termination
- No zombie processes or leaked file descriptors
- Lock files and caches properly released

**Multi-Server Resilience:**
- Hung server doesn't block shutdown of healthy servers
- Failed connections use fast path (skip LSP handshake)
- Global timeout prevents indefinite hang

### Negative

**No Response Draining:**
- Accepted writes drain to the server, but their responses are not
  awaited — pending operations fail when the timeout expires
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
is retained (§ Operation Disposal Policy): it is local, FIFO-bounded work,
distinct from waiting on a server.

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
- SIGKILL guarantees termination (last resort)

## Amendment History

- **2026-01-06**: Merged Amendment 001 - Added three-phase writer loop shutdown synchronization to prevent stdin corruption during concurrent shutdown writes
- **2026-08-11**: Corrected Initialization Shutdown - the abort path sends no LSP message at all (the earlier revision sent `exit` before the initialize response, which LSP ordering forbids); adopted alongside bridge-client-control-protocol, whose per-slot `stop` shares the path
- **2026-08-11**: Reconciled the Operation Disposal Policy with the Closing-state gating and the writer's actual behavior - the accepted order queue drains ahead of `shutdown` (the earlier table said queued operations are never sent, contradicting § Operation Gating and the FIFO writer)
