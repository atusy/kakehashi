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
bookkeeping-only `Closed` (§ Unconfirmed Termination).

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

#### Unconfirmed Termination

**Unconfirmed termination at the global deadline**: the connection still
transitions to `Closed` for bookkeeping. What happens to the child
depends on whether the kakehashi process is actually exiting:
log-and-abandon applies **only on the process-exit path** (the `exit`
notification, or teardown that ends the process) — and it is safe there
for exactly one reason: the parent's imminent exit reparents the child
to init, which reaps it, so neither a live straggler nor a zombie can
persist. A teardown after which kakehashi keeps running is by
definition `ServerRemains` and retains its records; delivery of SIGTERM
or SIGKILL never substitutes for the reaped `wait`. When a `Teardown(ServerRemains)`
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

**Problem**: The writer loop and the shutdown sequence both write to the
server's stdin. Concurrent writes corrupt the protocol stream.

**Decision**: The shutdown sequence takes **exclusive** access to stdin from
the writer before it sends `shutdown`, and waits only a bounded time to get
it. Failing to acquire it, the sequence skips the LSP handshake entirely and
force terminates.

**Sequence:**
```
1. Transition to Closing state (new operations rejected)
2. Close queue admission — accepted operations still drain
3. Writer drains the accepted queue in FIFO order, then yields stdin
4. Shutdown sequence writes `shutdown` / `exit` over the yielded stdin
   ── or, if step 3 did not complete within its bound, skips to ──
5. Force terminate (SIGTERM → SIGKILL)
```

Draining before the handshake is what makes `shutdown` unable to overtake a
message the bridge already committed to send (§ Operation Disposal Policy).
The bound on step 3 is the writer-idle timeout of
ls-bridge-timeout-hierarchy § Writer-Idle Timeout, which counts against the
applicable shutdown budget — per-slot `stop` or global teardown — rather than adding to it.

**Guarantees (graceful path — a forced termination forfeits the queue
drain and current-write completion; the bounded wait is what enforces
it):**
- ✅ Writer loop stops dequeuing **before** shutdown writes to stdin
- ✅ No concurrent writes to stdin (sequential: writer → shutdown)
- ✅ Bounded wait (no indefinite hang)
- ✅ Current write completes (no mid-write abortion)

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

**Timeout Application** (**target state**; see § Decision–Implementation
Gap): the ceiling is one absolute deadline split between the graceful phase
and an escalation reserve
(ls-bridge-timeout-hierarchy § Global Shutdown Design). It is owned by the
teardown transition (§ Lifecycle Actor below), which is also what makes it
un-extendable: the deadline is captured at shutdown initiation, so every
later step spends the budget rather than adding to it.

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
§ Invariants requires and § Connection State Tracking tabulates — the
actor's abort path wins or
loses through that same conditional commit, never around it.

This replaces the lock-based concurrent design an earlier revision of this
section specified (see Alternative 4): instead of *handling* the races
between stop, restart, teardown, reload, and acquire with dedicated
machinery, serialization **removes** them by construction.

**Decisions serialize; I/O does not.** The LSP shutdown handshake,
SIGTERM/SIGKILL escalation, and waiting on a child all run outside the
serialization point, reporting back as further messages. So the actor
never blocks on a server, and per-connection shutdowns still proceed in
parallel. A multi-step operation (`restart` = stop phase → respawn →
verify) is a per-key state machine advanced by those reports, which is
what makes every lifecycle state change atomic with respect to every
other one.

**Two shutdown modes**, which the teardown contracts below discriminate
on:

- `ProcessExit` — kakehashi itself is ending (exit notification, signal).
- `ServerRemains` — the LSP `shutdown` request path: the server answers,
  then stays alive awaiting `exit`, possibly indefinitely.

What the previous machinery bought, the actor gives structurally:

- **Single-flight per key** — two control calls for one key serialize;
  the second is answered from the key's current state
  (`clientRestarting`, `clientNotReady`, …). No registry of in-flight
  operations, and no half-mutated slot for a dropped handler to leave
  behind.
- **One lifecycle authority** — every spawn, signal, and wait happens
  under the actor's ownership, so there is nothing to lease, revoke, or
  reclaim, and no child can come into existence behind an escalation
  scan.
- **Spawn commit is an actor transition** — the acquire path may use an
  existing `Ready` connection without consulting the actor, but
  *creating* one may not: the stopped set, termination-pending records,
  in-flight operations, and teardown sealing are all checked in the same
  transition that commits the spawn. An acquire can never race a committing `stop` into
  spawning beside a tombstone.
- **Teardown is a transition, not a scan** — teardown carries the mode
  and an absolute deadline captured at shutdown initiation, seals
  admission, and drives the per-connection shutdowns. Sealing changes
  the *answers*, not the actor's availability: after the seal a spawn
  commit fails its acquire, and a `Stop`/`Restart` answers
  `clientNotReady` with `data.status: "stopping"`. A second teardown
  upgrades the mode monotonically (`ProcessExit` dominates) and never
  extends the ceiling — the earliest deadline offered wins. Completion
  publishes the run's **success-or-failure** only after cleanup, so
  callers surface or log a failed teardown rather than mistaking it for
  success. A `Teardown(ProcessExit)` arriving **after** a completed
  `ServerRemains` run is a new transition, not a lost upgrade: it adopts
  the retained records and disposes them by mode
  (§ Unconfirmed Termination). A run that reaches its final deadline with a
  spawn still unfinished publishes as **failed** and names the potential
  straggler in the log; its record stays authoritative until actual process
  exit ends ownership.
- **Abnormal outcomes settle in one place** — a spawn, handshake, or
  wait that ends abnormally settles through the same transition path as
  a normal one: it answers the outer request
  (`stopFailed`/`restartFailed`) and disposes the tombstone, ARM,
  replacement, and termination-pending records per
  bridge-client-control-protocol's ownership-at-completion rules.

**An acquire waiting on an in-flight spawn always terminates** — with the
connection, with the spawn's own error, or with the ordinary acquire
error for whatever terminal state the key reached: stopped, deleted by a
configuration reload, superseded by a reload that changed spawn-time
configuration, or sealed by teardown. It spends the *remaining* budget of
its original acquisition deadline, because internal churn must not reset a
caller's timeout, and an acquire that does time out drops only its own
wait — it disturbs no lifecycle record.

**Latency**: control and teardown events are rare and human-initiated;
the automatic transitions (spawn commits, respawn decisions, reload
purges) cost one round trip each — the per-spawn round trip already
recorded in bridge-client-control-protocol's consequences. Serializing
them costs nothing else observable, because only decisions serialize,
not process I/O.

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

**Coordination Strategy**, in order:

1. **Capture the absolute deadline**, before anything else — every later
   step spends this budget rather than extending it.
2. **Gate the router**: stop accepting new requests, which also gates
   ordinary acquires.
3. **Fail all pending routing decisions.**
4. **Hand the mode and deadline to the lifecycle actor's teardown
   transition** (§ Lifecycle Actor) and await its completion. Mode
   upgrade, sealing, the parallel per-connection shutdowns, escalation
   under the deadline, survivor disposition, and cleanup all belong to
   that transition. Router-specific resource cleanup runs before
   completion is published, so no caller can resume around it. A
   teardown that publishes as failed is surfaced or logged, never read
   as success.

The per-connection shutdowns still run in parallel — the actor serializes
the teardown *decisions* (mode, sealing, disposition), not the process
I/O, so the O(1) wall-clock property below is unaffected.

**Why Parallel:**
- **Bounded total time**: N servers shut down in O(1) time, not O(N)
- **Independent failures**: Hung server doesn't block others
- **User experience**: 3 servers × 5s sequential = 15s vs 5s parallel

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

**Process ownership**

- **A child process must never exist outside owned records.** The window is
  between creating the process and committing the record of it: an unwind,
  panic, or abandoned task there strands a child that nothing will ever
  signal or reap. It is not the only such window — a record abandoned while
  its spawn is still live reopens it from the other end
  (bridge-client-control-protocol).
- **Delivering SIGTERM or SIGKILL is not termination.** Only a reaped
  `wait` confirms a process is gone. Anything that treats signal delivery
  as confirmation will report success over a live straggler or accumulate
  zombies. A `wait` that keeps failing must keep retrying visibly and
  boundedly — never a silent promotion to closed, never a hot loop.
- **Terminating a child is not the same as ceasing to own it.** Ownership
  ends when the process is reaped, or when kakehashi's own imminent exit
  reparents the child to init — never merely because a deadline passed
  (§ Unconfirmed Termination).
- **The stopped-check and the spawn-decision must be atomic.** Split them
  and an acquire spawns a connection beside a tombstone a concurrent `stop`
  just committed, resurrecting a server the user asked to stay down.
- **Accepted work must always have exactly one worker.** Neither zero — a
  crash between accepting the work and starting it must not strand the
  record with nobody acting on it — nor two, which for a spawn means two
  children. This is the same hazard respawn-reopen-derives-its-targets
  addresses by deriving rather than remembering.
- **Lifecycle records that cannot be re-derived must outlive the component
  that owns them.** The stopped set is precisely the record of keys *not*
  in the pool, so losing it cannot be repaired by reading the pool;
  in-flight waits and termination-pending records are the same. Only state
  the pool can supply may be re-derived after a failure.

**Protocol integrity**

- **Until the server has answered `initialize`, LSP forbids the client every
  further request *and* notification** — `exit` included. The only
  conformant abort in that window is process termination with no LSP
  message at all.
- **The shutdown handshake requires exclusive access to stdin, and
  quiescence observed from outside is not exclusivity.** A writer that
  merely looks idle may be mid-frame; interleaving a `shutdown` request
  with a partial frame corrupts the JSON-RPC stream unrecoverably. Failing
  to acquire exclusivity, the correct action is to skip the handshake, not
  to write anyway.
- **Closing admission must be atomic against a stale view.** A caller
  holding a `Ready` view of a connection that has since begun closing must
  not be able to enqueue; otherwise an operation is accepted after the
  bridge has promised to accept none.
- **A blocked writer plus a downstream that has stopped reading is a
  full-duplex pipe deadlock.** Draining the accepted queue is bounded in
  *work*, not in *time* — so every drain must run under a deadline and be
  able to end in forced termination.

**Answering callers**

- **No caller is left pending, and settlement is at-most-once.** Every
  control operation ends in exactly one answer, including when the work
  behind it ends abnormally rather than reporting.
- **A failure answer must be truthful.** A caller told
  `stopFailed`/`restartFailed` must be able to read it as *the terminal
  outcome was not committed* — a committed success must never be reported
  as a failure.
- **A deadline is a ceiling, not a budget that internal events refill.**
  Queueing delay, retries, and configuration churn spend it; nothing
  extends it, and no operation may outlive its ceiling because the expiry
  signal itself was lost.
- **A completion or an expiry must be attributable to the operation it was
  armed for**, and one that no longer matches the key's current operation
  changes nothing. Otherwise a timed-out `stop`'s late expiry settles or
  kills the `restart` that replaced it, and a superseded spawn's completion
  answers on behalf of its successor.

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
  process-exit path — see § Unconfirmed Termination)
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

## Decision–Implementation Gap

The LSP handshake, the writer handoff with its queue drain, the
SIGTERM → SIGKILL escalation, and parallel teardown are implemented. Three
parts of this decision run ahead of the code, which is the ordinary state of
an ADR here:

- **The lifecycle actor does not exist yet.** Teardown today is a pool-wide
  shutting-down flag checked under the connections lock — enough to make the
  stopped-check/spawn-decision atomic for teardown, but there is no per-slot
  `stop`/`restart`, so most of the serialization this section describes has
  nothing yet to serialize. It converges as bridge-client-control-protocol
  lands.
- **The wait for the writer handoff is unbounded per connection.** Nothing
  inside the connection bounds it; only the enclosing teardown budget does —
  the graceful ceiling, or the force-kill bound if escalation reaches it. So
  one wedged writer can spend the whole graceful budget instead of its own
  writer-idle share.
- **The escalation reserve does not exist.** The ceiling bounds the graceful
  phase *only*; force-kill then runs after it with its own additive
  per-connection budget, which itself contains a SIGTERM grace period. A
  teardown can therefore overrun the ceiling it is supposed to be bounded by
  — with the shipped defaults, by roughly a third.

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
- **2026-08-12**: Applied the contract/invariant/mechanism discipline (template.md) - deleted the lifecycle-actor and writer-handoff implementation mechanics (escrow slots, kill-on-drop guards, scratch-copy staging, commit-and-reply swaps, generation-bound receivers, settlement markers, the message-enum and coordination sketches, the writer-idle constant), and added an Invariants section recording the traps that machinery closed. Replaced the aspirational-design note with a Decision–Implementation Gap section, dropping its stop-oneshot and writer-return-channel divergences as no longer load-bearing and adding the lifecycle-actor and escalation-reserve gaps. No contract changed.
