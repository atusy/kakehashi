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
Closing → Closed         (shutdown completed or timed out — see below)
Failed → Closed          (skip shutdown handshake, cleanup only)
```

`Closing → Closed` on timeout is **mode-scoped**: a per-slot `stop`
(bridge-client-control-protocol) reaches `Closed` only after `Child::wait`
returned `Ok` — an unconfirmed termination converts to a
termination-pending record (`stopping`) and settles
`stopFailed`/`restartFailed` instead. Only global teardown may take the
bookkeeping-only `Closed` (§ Unconfirmed termination).

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
│ 4. Transition to Closed state (per-slot: only after     │
│    wait returns Ok — see mode-scoped note above)        │
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
notification, or teardown that ends the process). When a `Teardown(ServerRemains)`
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

> **Target design.** The sketches in this section (and § Lifecycle Actor /
> § Multi-Connection Shutdown below) record the protocol adopted
> alongside bridge-client-control-protocol. The current implementation
> differs — a stop oneshot with `try_recv` drain, a separate writer
> return channel, no per-connection handoff timeout — and converges as
> that protocol lands (adrs-are-aspirational is the repo's standing
> convention: ADRs run ahead of code).

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

// Writer loop — owns its state by value (incl. stdin); ownership is what
// makes the handoff transfer real (target design — see note above)
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

**Global timeout**: Implementation-defined duration (typically 5-15 seconds) bounding the termination *attempt* (escalation) across all connections. Ownership disposition and local cleanup (actor state, router state) are non-suspending actor transitions needing no server cooperation; they run immediately after and fall outside the ceiling.

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

**Timeout Application**: see § Lifecycle Actor below — the deadline and the
escalation reserve are owned by the actor's teardown state machine.

### Lifecycle Actor: All Lifecycle Transitions Serialize

**Decision**: one pool-owned **lifecycle actor** owns every lifecycle
transition — per-slot `stop`/`restart` (bridge-client-control-protocol),
pool teardown, spawn commits, respawn decisions, tombstones,
termination-pending records, and configuration-reload connection purges
with their settings publication. Callers send messages and await replies;
reads (enumeration, id resolution, acquire routing checks) go through a
snapshot the actor publishes atomically per transition; nothing else
mutates lifecycle state. The one deliberate boundary: per-connection
*handshake* terminal commits (`Initializing → Ready/Failed/Closing`)
remain the conditional compare-transitions ls-bridge-message-ordering
§ Connection State Tracking defines — the actor's abort path wins or
loses through that same conditional commit, never around it.

This replaces the lock-based concurrent design an earlier revision of this
section specified (see Alternative 4): instead of *handling* the races
between stop, restart, teardown, reload, and acquire with dedicated
machinery, serialization **removes** them by construction.

**The actor loop never blocks.** Long operations — the LSP shutdown
handshake, SIGTERM/SIGKILL escalation, `Child::wait` — run as spawned
sub-tasks that own only what the transition hands them (a process handle,
the connection's I/O endpoints for a handshake, a reply channel) and no
lifecycle state; each completion comes back to the actor as a message. A
multi-step operation (`restart` = stop phase → respawn → verify) is a
small per-key state machine advanced by those messages, so every state
mutation is atomic at message granularity and there is no lock to hold,
tear, or lease.

What the previous machinery bought, the actor gives structurally:

- **Single-flight per key** — two control calls for one key are two queued
  messages; the second is answered from the key's current state
  (`clientRestarting`, `clientNotReady`, …). No registry, no guard to
  release, no half-mutated slot for a dropped handler to leave behind.
- **No lease-owner map** — the actor is the only entity that spawns,
  signals, or waits a process: because spawning itself is a non-suspending
  actor transition, no child can come into existence behind an escalation
  scan. Sub-tasks the actor spawns are the delegation, tracked in its
  `JoinSet`; a "revoked claim" is just the actor ignoring a stale
  completion message.
- **Spawn commit is an actor transition** — the acquire path may use an
  existing `Ready` connection lock-free, but *creating* one goes through
  the actor, which checks the stopped set, termination-pending records,
  in-flight operations, and teardown sealing in-queue. An acquire can
  never race a committing `stop` into spawning beside a tombstone.
- **Teardown is a message** — `Teardown(mode)` seals admission and drives
  the parallel per-connection shutdowns as sub-tasks. Sealing means the
  actor *answers differently*, not that it stops reading its queue: a
  post-seal spawn commit fails the acquire, and a post-seal
  `Stop`/`Restart` answers `clientNotReady` with
  `data.status: "stopping"`. The budget is one absolute deadline with an
  escalation reserve (~20%, ls-bridge-timeout-hierarchy): the graceful
  phase runs to `deadline − reserve`, escalation gets the remainder, and
  each `DeadlineExpired` token carries the operation generation it was
  armed for — a token whose generation no longer matches is stale and
  dropped. A second `Teardown` upgrades the mode monotonically
  (`ProcessExit` dominates); the actor owns the single completion watch,
  hands every caller a receiver, and publishes only after cleanup.
- **Abnormal outcomes settle in one place** — the actor polls its
  sub-task `JoinSet` alongside the mailbox, so a sub-task that panics
  (and therefore never sends its completion) still surfaces as a
  `join_next` result; the actor applies the ownership-at-completion rules
  (bridge-client-control-protocol) to its own state entry: settle the
  tombstone/ARM/replacement records, answer the outer request
  (`stopFailed`/`restartFailed`), keep or convert the termination-pending
  record.

**The actor's state outlives the actor task.** `LifecycleState` and the
sub-task `JoinSet` live in pool-owned storage handed to each incarnation,
never in task locals — a panicking incarnation can drop neither the
stopped set (which is precisely the record of keys *not* in the pool and
so cannot be re-derived from it), nor termination-pending kill handles,
nor in-flight `Child::wait`s. The pool's supervisor restarts the actor;
the new incarnation resumes from that storage plus its mailbox, and
re-derives only what pool state can supply (connection states) — the same
derive-don't-remember posture as respawn-reopen-derives-its-targets. The
one thing a dying incarnation *can* drop is a reply channel it was about
to answer; the caller's closed receiver maps to `RequestFailed`
(`stopFailed`/`restartFailed`), so settlement is at-most-once and no
caller is ever left pending.

**Sketch** (illustrative, target design):

```rust
enum ShutdownMode {
    ProcessExit,   // exit notification / signal path: kakehashi is ending
    ServerRemains, // LSP shutdown request: server stays alive awaiting exit
}

enum LifecycleMsg {
    Stop { key: ConnectionKey, reply: Reply },
    Restart { key: ConnectionKey, reply: Reply },
    CommitSpawn { key: ConnectionKey, reply: Reply },   // acquire's spawn path
    Reload { generation: ConfigGeneration, reply: Reply },
    Teardown { mode: ShutdownMode, reply: Reply<WatchRx> }, // actor owns the one watch
    DeadlineExpired { token: DeadlineToken },           // generation-stamped
}

async fn lifecycle_actor(pool: Arc<ConnectionPool>) {
    // State and JoinSet are POOL-OWNED, handed to each incarnation —
    // a panicking incarnation drops neither records nor in-flight waits.
    let (mut rx, state, tasks) = pool.lifecycle_storage();
    loop {
        select! {
            Some(msg) = rx.recv() => state.step(msg, &pool, tasks),
            Some(done) = tasks.join_next() => state.settle(done, &pool),
            // Every arm is non-suspending state mutation plus sub-task
            // spawns; long I/O never runs inside the loop. A panicked
            // sub-task never sends a completion message — join_next is
            // how it still surfaces.
        }
    }
}
```

**Latency**: lifecycle transitions are rare, human-initiated events;
serializing them costs nothing observable. The per-connection shutdown
handshakes themselves still run in parallel as sub-tasks — the actor
serializes *decisions*, not process I/O.

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
└─ Transition: Closing → Closed (handle bookkeeping; per-slot slot status
   stays `stopping` via a termination-pending record until wait returns Ok)
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
    // 1. Stop accepting new requests (also gates ordinary acquires via
    //    the pool-wide shutting_down flag)
    mark_router_shutting_down();

    // 2. Fail all pending routing decisions
    fail_pending_routes();

    // 3. Send Teardown(mode) to the lifecycle actor (§ Lifecycle Actor)
    //    and await the shared completion watch. Mode upgrade, sealing,
    //    the parallel per-connection shutdown sub-tasks, escalation
    //    under the absolute deadline, survivor disposition, and cleanup
    //    are all the actor's teardown state machine; router-specific
    //    resource cleanup runs inside its cleanup step, BEFORE
    //    completion is published, so no caller can resume around it.
    pool.lifecycle().teardown(mode).await;
}
```

The per-connection shutdowns themselves run in parallel as actor
sub-tasks — the actor serializes the teardown *decisions* (mode, sealing,
disposition), not the process I/O, so the O(1) wall-clock property below
is unaffected.

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
- Global timeout bounds the termination *attempt* — escalation only, not
  confirmed termination; ownership disposition and local cleanup run
  immediately after, outside the ceiling, and need no server cooperation
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

### Alternative 4: Lock-Based Concurrent Lifecycle Control

Let stop, restart, teardown, reload, and acquire run concurrently against
shared pool state, and close each race with dedicated machinery. An
earlier revision of this decision specified exactly that, and adversarial
review drove it to its logical conclusion: a single-flight
control-operation registry, a central lease-owner map with
revoke-and-acknowledge handoff, supervisor-owned transactional teardown
state with phase markers and rollback-on-unwind, a durable-record
finalizer with an always-running reaper, kill-on-register live process
registries, and owner/joiner teardown arbitration with monotonic mode
upgrade.

**Rejected Reasons:**

1. **Mechanism per race, forever**: every closed interleaving exposed the
   next one; ~20 review rounds added machinery without converging,
   because the concurrency the machinery serves is the problem
2. **Weight mismatch**: the residual risk left after serialization (a
   transient zombie, a redundant signal to a dead pid, a slot wedged
   until restart) never justified *dedicated* database-grade ownership
   machinery on top
3. **Foreign to the codebase**: kakehashi already solves this class with
   actors (parse actor, writer task, response router); a lock-and-lease
   subsystem would be the odd one out
4. **Implementation distance**: none of it existed; an implementer would
   be committed to the full lattice before the first feature shipped

**Why the actor is better**: serialization removes the races instead of
handling them; the observable contracts (single-flight answers,
tombstone/termination-pending semantics, teardown mode upgrade,
settlement of abnormal outcomes) survive unchanged as consequences of one
queue. The actor and its supervisor are new code too — but one
well-worn shape instead of five bespoke mechanisms — and latency is
unaffected because only decisions serialize, not process I/O.

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
- **2026-08-12**: Replaced the lock-based concurrent lifecycle-control design with the Lifecycle Actor - all lifecycle transitions (stop/restart/teardown/spawn-commit) serialize through one pool-owned actor, dissolving the single-flight registry, lease-owner map, supervisor-owned transactional teardown state, and durable-record finalizer machinery the earlier revision had accreted (now recorded as rejected Alternative 4); observable contracts in bridge-client-control-protocol are unchanged
