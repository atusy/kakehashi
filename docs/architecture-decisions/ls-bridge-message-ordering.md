# LS Bridge Message Ordering

**Phasing**: See ls-bridge-implementation-phasing — This decision covers Phase 1; optional coalescing deferred to Phase 2.

## Scope

This decision defines message ordering guarantees for **a single connection** to a downstream language server. It covers:
- Single-writer actor loop for protocol correctness
- Connection state machine (Initializing → Ready → Failed/Closing → Closed)
- Operation gating based on connection state
- Cancellation forwarding to downstream servers

**Out of Scope**: Coordination of multiple connections (routing, aggregation) is covered by ls-bridge-server-pool-coordination.

## Context

### Problems with Previous Approach

An earlier design established timeout-based control for initialization and request superseding. This approach had three fundamental problems:

**1. Time-Based Control Doesn't Reflect System State**

Timeouts create artificial ceilings unrelated to actual readiness:
- Fixed timeout dilemma: too short fails unnecessarily, too long wastes user time
- Server variability: lua-ls initializes in 100ms, rust-analyzer takes 5-10s
- No feedback: timeout expiry doesn't indicate when server will actually be ready

**2. Notification/Request Ordering Violation**

Separate code paths create race conditions where requests can arrive before notifications, leading to completion on stale content. This is hidden by content-hash URIs but becomes catastrophic with stable URIs.

**3. Complexity from Per-Type State Management**

Timeout tracking requires separate pending maps per request type, each with its own timeout task, cancellation logic, and cleanup.

### Key Architectural Insight

A thin bridge that forwards requests and relies on client-driven cancellation:
- Single-writer loop serializes all writes (prevents protocol corruption)
- Clients manage stale requests via `$/cancelRequest` (LSP standard)
- Downstream servers handle concurrent requests efficiently
- Bridge stays simple: forward requests, forward responses, forward cancellations

**End-to-end principle**: Don't add complexity in the middle layer for something the endpoints already handle.

## Decision

**Adopt actor-based message ordering with a thin forwarding bridge**, structured around five architectural principles.

### Architecture Overview

```
┌──────────────────────────────────────────────────────────┐
│              Per-Connection Actor Pattern                │
│                                                          │
│  Unified Operation Stream:                               │
│  ┌──────────────────────────────────────────────────┐    │
│  │ Notifications + Requests                         │    │
│  │   (didChange, hover, completion, etc.)           │    │
│  └─────────────────┬────────────────────────────────┘    │
│                    │                                     │
│                    ▼                                     │
│  ┌──────────────────────────────────────────────────┐    │
│  │           Unified Order Queue (FIFO)             │    │
│  │  Bounded admission threshold (4096; 256 caused   │    │
│  │  routine fan-out loss)                           │    │
│  │  - Ensures FIFO ordering                         │    │
│  │  - Non-blocking backpressure                     │    │
│  └─────────────────┬────────────────────────────────┘    │
│                    │                                     │
│                    ▼                                     │
│  ┌──────────────────────────────────────────────────┐    │
│  │         Single Writer Loop (Actor)               │    │
│  │  - Dequeues from order queue                     │    │
│  │  - Writes to server stdin                        │    │
│  │  - Tracks pending requests for correlation       │    │
│  └─────────────────┬────────────────────────────────┘    │
│                    │                                     │
└────────────────────┼──────────────────────────────────────┘
                     ▼
              Server stdin (serialized)
```

### 1. Single-Writer Loop (Actor Pattern)

Each server connection has exactly one writer task consuming from a unified queue, ensuring FIFO ordering for all operations.

**Key Properties:**
- Strict FIFO ordering (notifications and requests maintain sequence)
- No byte-level corruption (single writer, no interleaving)
- Prevents notification/request race (all flow through same channel)

### 2. Request Forwarding (Thin Bridge)

Requests are forwarded directly to downstream servers without method/URI
coalescing or bridge-inferred supersession. One exception honors an explicit
upstream `$/cancelRequest`: a tracked request still waiting in the writer queue is
skipped before any bytes are written; a request whose write has started is followed
by the cancel notification through the same FIFO queue.

**Rationale**:
- Upstream clients manage stale requests via `$/cancelRequest`
- Downstream servers handle concurrent requests efficiently
- Simplicity over premature optimization

**Request/Response Flow:**

```
Client                    Bridge                      Downstream
  │                         │                             │
  │──hover(host-uri, pos)──▶│──hover(virtual-uri, pos')──▶│
  │                         │   (transform URI & position)│
  │                         │                             │
  │◀──result(host-uri, pos)─│◀──result(virtual-uri, pos')─│
  │    (transformed)        │   (transform URI & position)│
```

**Bridge Responsibilities:**
- **Outbound**: Transform host URI → virtual URI, map positions (host → virtual)
- **Inbound**: Transform virtual URI → host URI, map positions (virtual → host)
- **Correlation**: Match response to pending request by ID

**Writer Loop:**

```rust
// Simple forwarding loop
loop {
    let operation = order_queue.recv().await;

    // Track for response correlation
    if operation.is_request() {
        pending_requests.insert(operation.id, response_channel);
    }

    // Transform and forward to downstream server
    let transformed = transform_outbound(operation);
    write_to_stdin(transformed).await?;
}
```

**Pending Request Tracking:**

The bridge tracks pending requests for response correlation only:
- `pending_requests: HashMap<RequestId, ResponseChannel>`
- Entry added when request sent to downstream
- Entry removed when response received or connection closes
- Memory bounded by O(concurrent requests), not O(historical requests)

**Future Extension (Phase 2)**: If profiling shows excessive load from rapid-fire requests, add optional coalescing with generation counters. See Future Considerations.

### 3. Non-Blocking Backpressure

The order queue is bounded and admission never blocks, so a slow or
initializing server cannot deadlock a producer. **Admission threshold: 4096**
— raised from the original 256, which caused routine fan-out loss. Held and
reserved traffic count toward that depth, so **while the combined depth sits
above the threshold, admission stays closed until the writer drains below
it** — the threshold governs when a producer is refused, not merely how much
is allocated. The queue's memory bound is testable and covers responses as
well as outbound traffic.

Responses to **downstream-initiated** requests are the delicate case, because
the response router deliberately does not track them. So an inbound server
request is either admitted with room for its answer already accounted for, or
**answered up front with `RequestFailed`** rather than accepted — never
accepted and then dropped for want of room. A refusal serializes inbound admission behind its own
sending, and that wait is **bounded and shutdown-aware**: the deadline is the
existing 5-second-class response-send bound, capped by the earliest active
lifecycle deadline. On expiry the connection fails via **conditional
compare-transition from its current state** — `Initializing → Failed` (the
path is reachable during initialization through permitted
`window/showMessageRequest` traffic) or `Ready → Failed`, while a shutdown
that already won keeps `Closing`/`Closed`. Within the bound, overflow applies
pipe backpressure to the downstream instead of ever dropping silently.

**Strategy by Operation Type:**

| Operation Type | Queue Full Behavior | Rationale |
|---------------|-------------------|-----------|
| **Notifications** (didChange, didSave, etc.) | Drop with telemetry | Extreme backpressure, recoverable via next notification |
| **Requests** | Explicit error | Return `REQUEST_FAILED` |

**Notification Drop Telemetry:**
- Log at WARN level (always)

**Future Extension (Phase 2)**: Full telemetry (`$/telemetry` events).

### 4. Connection State Tracking

Explicit connection state enum separates data flow from control flow.

```rust
enum ConnectionState {
    Initializing,  // Writer loop started, initialization in progress
    Ready,         // Initialization completed successfully
    Failed,        // Initialization failed or writer loop panicked
    Closing,       // Shutdown initiated, draining operations (ls-bridge-graceful-shutdown)
    Closed,        // Connection terminated (bookkeeping-only at the global
                   // deadline: an unconfirmed child may be retained or
                   // abandoned — ls-bridge-graceful-shutdown)
}
```

**State Machine:**

```
                      ┌─────────────┐
                      │Initializing │
                      └──────┬──────┘
                             │
           ┌─────────────────┼───────────────────┐
           │                 │                   │
        success       timeout/failure/       shutdown
           │           crash/panic             signal
           ▼                 │                   │
      ┌────────┐             │                   │
      │ Ready  │             │                   │
      └───┬────┘             │                   │
          │                  │                   │
     ┌────┴───────────────┐  │                   │
     │                    │  │                   │
 shutdown             crash/ │                   │
  signal              panic  │                   │
     │                    │  │                   │
     ▼                    ▼  ▼                   ▼
┌─────────┐             ┌────────┐          ┌─────────┐
│ Closing │             │ Failed │          │ Closing │
└────┬────┘             └────┬───┘          └────┬────┘
     │                       │                   │
     │                       │                   │
     │                       │ (direct to        │
     │                       │  Closed)          │
     └───────────────────────┼───────────────────┘
                             │
                             ▼
                       ┌──────────┐
                       │  Closed  │  (terminal state)
                       └──────────┘
```

**Transition Summary:**

| From | Trigger | To |
|------|---------|-----|
| `Initializing` | success — commits atomically with the `initialized` enqueue (bridge-client-control-protocol) | `Ready` |
| `Initializing` | timeout / failure / crash / panic | `Failed` |
| `Initializing` | shutdown signal | `Closing` |
| `Ready` | shutdown signal | `Closing` |
| `Ready` | crash / panic | `Failed` |
| `Failed` | cleanup / shutdown (not automatic) | `Closed` |
| `Closing` | (graceful or timeout) | `Closed` |
| `Closing` | panic | `Closed` |

The **`Closing` → `Closed`** timeout and panic rows (and only those —
initialization timeout/failure/crash/panic go to pool-resident `Failed`,
exactly as the table says) mark the **handle** `Closed` unconditionally —
that is connection-object bookkeeping. Slot-level termination
confirmation is tracked separately: when the process is unconfirmed, a
termination-pending record keeps the *slot* at `stopping` even though the
handle reads `Closed` (bridge-client-control-protocol; enumeration
ignores `Closed` handles and lets the record answer).

`Failed` handles stay pool-resident — addressable and enumerable — until a
later acquire replaces them or cleanup/shutdown closes them;
bridge-client-control-protocol relies on that residency for its `failed`
status and `restart` target resolution.

**Key Transition: Failed → Closed (Direct)**

`Failed` transitions directly to `Closed`, bypassing `Closing`. This is because:
- `Failed` state means the connection is already broken (panic, crash, timeout)
- LSP shutdown handshake is impossible (stdin may be unavailable)
- ls-bridge-graceful-shutdown specifies: `Failed → Closed` with process cleanup only (SIGTERM → SIGKILL)

**Operation Gating:**

Operations are gated at two levels: **server lifecycle** and **document lifecycle**.

**Server Lifecycle Gating** (ConnectionState):
- Writer loop starts immediately in `Initializing` state
- **Requests**: Gated on `Ready` state:
  - `Initializing` → `REQUEST_FAILED` ("bridge: downstream server initializing")
  - `Failed` → `REQUEST_FAILED` ("bridge: downstream server failed")
  - `Closing` → `REQUEST_FAILED` ("bridge: connection closing") [See ls-bridge-graceful-shutdown]
  - `Closed` → `REQUEST_FAILED` ("bridge: connection closed")
- **Notifications**: Accepted by writer loop in `Initializing` or `Ready` state only
  - **Target semantics, adopted with bridge-client-control-protocol**
    (required before that protocol's pass-through ships): during
    `Initializing`, accepted notifications are **held** rather than
    written, and only handshake-owned traffic reaches the wire until
    `initialized` has been written — a strict single FIFO could not
    reorder traffic accepted before the initialize response behind the
    later `initialized`. Exactly two exemptions, categorized separately:
    the **conforming exception** — responses to
    `window/showMessageRequest`, the one server-initiated request LSP
    permits before the initialize response, go to the wire immediately,
    because withholding them could deadlock the handshake — and a
    **tolerated extension**: a `window/workDoneProgress/create` in this
    window is a nonconforming server, which the bridge nevertheless
    answers (logged), since refusing could wedge initialization.

    Held notifications release in arrival order when the connection
    becomes `Ready`, and that release can neither be refused nor fail
    (§ Invariants). The hold introduces **no observable state**: a
    connection reads `Initializing` until it reads `Ready`, with nothing
    between, so no `flushing`-like status ever appears in the
    ConnectionState enum or in the client enumeration
    (bridge-client-control-protocol). What may legally enter the hold is narrow.
    **Document lifecycle notifications keep their Ready-only gating and
    are never held**: they arrive via the post-Ready derived open paths,
    and releasing a held `didChange` ahead of the derived `didOpen`
    would violate the document lifecycle. **State-replacement
    notifications are not held either.** The pool retains current
    settings and pushes the latest value after `initialized` ("latest"
    within downstream-settings-propagation's accepted race: a reload
    landing mid-handshake may push the previous revision, converging on
    the next propagation) — holding stale copies would invert
    latest-value semantics. `workspace/didChangeConfiguration` and
    `$/setTrace` both have that shape and both follow that policy:
    coalesced to latest, pushed post-`initialized`, never held. With
    document sync Ready-gated and every latest-value method coalesced,
    **no currently specified method is admissible to the hold** — it is
    expected empty, and exists as the structural safety net for future
    sequence-dependent, non-document notifications. It is bounded, with
    drop-newest-and-warn overflow
  - `Closing`/`Closed` → DROP (writer loop stopped, see ls-bridge-graceful-shutdown)
  - Subject to document lifecycle gating below

**Why `REQUEST_FAILED` instead of `SERVER_NOT_INITIALIZED`**: The upstream client communicates with kakehashi, which IS initialized. The client has no knowledge of downstream servers—that's an internal implementation detail. Using `SERVER_NOT_INITIALIZED` would confuse clients that just received an `initialized` response from kakehashi.

**Document Lifecycle Gating** (per downstream, per URI):

LSP requires `didOpen` before any document-specific operations. Two-level gating ensures correct ordering:

| Operation | Server Lifecycle | Document Lifecycle |
|-----------|------------------|-------------------|
| `didOpen` | Requires `Ready` | Transitions NotOpened → Opened |
| `didChange`, `didSave`, `willSave` | Requires `Ready` | Requires `Opened` (DROP if NotOpened) |
| Document requests (hover, etc.) | Requires `Ready` | Requires `Opened` |

**Key constraint**: `didOpen` is only sent **after** the server reaches `Ready` state. This ensures:
1. LSP handshake completes before document notifications
2. `didOpen` contains the current document snapshot (not stale content)

The `didOpen` notification contains the complete accumulated state, making queued `didChange` notifications redundant.

### 5. Cancellation Forwarding

Cancellation from upstream (via `$/cancelRequest`) is forwarded to downstream servers.

**Cancellation Flow:**

```
Client                    Bridge                      Downstream
  │──$/cancelRequest(42)──▶│──$/cancelRequest(42)────▶│
  │                        │                          │ (server decides)
  │◀──error or result──────│◀──error or result────────│
  │  (transformed)         │  (transform response)    │
```

**Bridge Behavior:**
- Atomically classify each tracked request as `Queued`, `Writing`, or `Sent`
- For `Queued`, mark it cancelled and let the single writer skip it, answering the
  local waiter with `REQUEST_CANCELLED`; no downstream cancel is needed because the
  request never reached the server
- For `Writing`/`Sent`, enqueue `$/cancelRequest` through the same writer FIFO
- Keep a sent request's pending entry (response still expected)
- Forward whatever response the server sends (result or REQUEST_CANCELLED error)

**Rationale**: An explicit client cancellation is authoritative; forwarding a
request that is provably still queued wastes downstream CPU and puts its later
cancel behind the very work it should prevent. The writer-owned atomic claim closes
that queue race without a priority lane, reordering, method-specific staleness
heuristics, or generation map. Once writing begins, downstream servers retain the
standard choice: they either:
- Complete the request (too late to cancel) → forward result
- Cancel successfully → forward REQUEST_CANCELLED error

**Coordination with ls-bridge-server-pool-coordination:** Router forwards `$/cancelRequest` to all connections that received the original request.

### 6. Fail-Fast Error Handling

Writer loop panics use fail-fast pattern (not restart) because `ChildStdin` cannot be cloned.

**Strategy:**
- Panic caught, all pending operations failed with INTERNAL_ERROR
- Connection state transitions to `Failed`
- No restart attempt (stdin consumed, restart creates silent permanent hang)
- Connection pool spawns new server instance with fresh stdin

**Recovery time**: ~100-500ms (respawn) vs. infinite hang (restart attempt).

**Failed State Semantics:**
- `Failed` is a terminal state for the *connection* (no self-recovery)
- The *pool* decides the response: respawn new connection (normal) or cleanup (shutdown)
- During shutdown: `Failed → Closed` (see ls-bridge-graceful-shutdown), no respawn

**Panic Handler Order:**
1. **First**: Fail all pending operations (LSP response guarantee)
2. **Second**: Transition to `Failed` state (or `Closed` if already `Closing`)

**Special Case**: Panic during `Closing` state → `Closed` (not `Failed`).

**Cross-Task Panic Propagation:**
When the writer task panics, the reader task must also exit to prevent CPU spin on orphaned channels:
- Use a shared `CancellationToken` (e.g., `tokio_util::sync::CancellationToken`)
- Writer panic handler: (1) fail pending with `INTERNAL_ERROR`, (2) cancel token, (3) transition state
- Reader task includes `token.cancelled()` in its `select!` loop
- Reader exits when token is cancelled, allowing connection respawn
- **LSP guarantee**: All pending requests receive `INTERNAL_ERROR` response before reader exits

```rust
// Reader task select! loop
select! {
    result = reader.read_message() => { /* handle response */ }
    _ = token.cancelled() => { break; }  // Writer panicked, exit
    _ = shutdown_rx.recv() => { break; } // Graceful shutdown
}
```

`read_message` must be cancel-safe. Every other branch completing first drops
the read future, so a parser holding frame progress in future-local state
loses a consumed header and resyncs onto the next message body — reporting a
framing error against a well-framed stream. See
`BridgeReader::read_message_bytes`, which keeps that progress in the reader and
awaits only cancel-safe primitives.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

**The wire**

- **At most one task writes a connection's wire at any instant.** Interleaved frames
  corrupt the JSON-RPC stream unrecoverably, and a corrupted stream is not
  something a peer can resynchronize from — this is why a panicked writer
  respawns the connection instead of being restarted (Alternative 3).
- **A paused reader plus a writer blocked on the server's stdin is a
  full-duplex pipe deadlock.** Anything that stops the reader — including
  waiting for a refusal to be sent — must therefore be bounded and
  shutdown-aware.

**Admission**

- **Admission control must never lose a message the bridge already committed
  to send.** For downstream-initiated requests that means one of two things
  happens and there is no third: the request is answered with `RequestFailed`
  instead of being accepted, or it is accepted and remains answerable.
  Accepting and then discovering there is no room for the answer leaves the
  server waiting forever.
- **Releasing held traffic cannot be refusable.** Its capacity is accounted
  when it is held, not when it is released; a release that can fail turns a
  bounded queue into silent notification loss at the one moment the
  connection is most fragile.

**The `Initializing` boundary**

- **Every transition out of `Initializing` commits conditionally.** Handshake
  success, initialization timeout, writer failure, and shutdown can all fire
  independently; whichever commits first must win permanently, or a
  connection that already lost the race writes on behalf of a teardown in
  progress.
- **Publishing `Ready`, enqueueing `initialized`, and releasing held
  notifications are one atomic act.** Split them and a handshake that lost
  the race still flushes, or a producer that observed `Ready` reaches the
  wire ahead of older held notifications — reordering exactly the traffic the
  hold exists to keep in order.
- **Until the server has answered `initialize`, LSP forbids the client every
  further request and notification.** Only handshake-owned traffic and the
  two recorded exemptions may reach the wire in that window.

**Documents**

- **Ordering `didOpen` first is not enough; it must also carry the current
  snapshot.** The ordering itself is contract (§ Document Lifecycle Gating);
  the trap is that a correctly-ordered `didOpen` built from content captured
  earlier desynchronizes the document for as long as it stays open.

## Consequences

### Positive

**Simplicity (Thin Bridge):**
- No coalescing map, no generation counters, no superseding logic
- Just forward requests, forward responses, forward cancellations
- Easier to understand, test, and debug

**Guaranteed Message Ordering:**
- Unified queue ensures notifications and requests maintain order
- Eliminates didChange → completion race condition
- Critical for stable URIs (PBI-200)

**End-to-End Principle:**
- Clients already handle stale request management via `$/cancelRequest`
- Servers already handle concurrent requests efficiently
- Bridge doesn't duplicate endpoint responsibilities

**Multi-Server Coordination:**
- State tracking enables router to skip uninitialized servers
- No spurious protocol errors from requests to uninitialized servers

**Robust Error Handling:**
- Deadlock prevention via non-blocking backpressure
- Silent hang prevention via fail-fast panic handling
- Explicit errors enable graceful degradation

**LSP Compliance:**
- Every request receives response (result or error)
- Standard cancellation flow via `$/cancelRequest`
- Maintains protocol semantics

### Negative

**Connection-Level Failure:**
- Writer loop panic fails entire connection (not just one operation)
- Requires connection pool to spawn new instance
- Trade-off: Better than silent permanent hang

**Notification Dropping Under Extreme Backpressure:**
- Notifications can be dropped if queue full (4096+ operations at the admission threshold)
- Only under extreme conditions
- Recoverable via subsequent notifications

**No Bridge-Level Superseding:**
- Rapid-fire requests all forwarded to server
- Server load may increase compared to coalescing approach
- Mitigation: Most servers handle this efficiently; add coalescing in Phase 2 if profiling shows need

### Neutral

**Backward Compatibility:**
- External LSP interface unchanged
- Internal refactor only

## Alternatives Considered

### Alternative 1: Bridge-Level Coalescing (Generation-Based Superseding)

Bridge maintains a coalescing map with generation counters to supersede stale requests before forwarding.

**Not Chosen For Phase 1:**

1. **Duplicates client responsibility**: Clients already send `$/cancelRequest` for stale requests
2. **Additional complexity**: Coalescing map, generation counters, atomic claim pattern
3. **Premature optimization**: Most servers handle concurrent requests efficiently
4. **Memory overhead**: Must track per-(URI, method) state

**Comparison:**

| Aspect | Coalescing | Thin Bridge (chosen) |
|--------|------------|----------------------|
| **Complexity** | Coalescing map + generations | Simple forwarding |
| **Memory** | O(unique URIs × methods) | O(concurrent requests) |
| **Superseding** | Bridge decides | Client decides via `$/cancelRequest` |
| **Server load** | Reduced (only latest sent) | All requests forwarded |

**Future Extension (Phase 2)**: If profiling shows excessive server load from rapid-fire requests, add optional coalescing.

### Alternative 2: Dual Channels (Separate Notification/Request Paths)

Maintain separate channels for notifications and requests.

**Rejected Reasons:**

1. **Ordering violation**: Requests can overtake notifications, causing stale content issues
2. **Critical with stable URIs**: Race condition becomes catastrophic (PBI-200)
3. **Complexity**: Two code paths, two sets of backpressure handling
4. **No FIFO guarantee**: Must manually coordinate ordering

**Why single channel is essential**: LSP semantics require `didChange` to be processed before subsequent `completion` on the same URI.

### Alternative 3: Writer Loop Restart on Panic

Attempt to restart the writer loop after panic instead of failing the connection.

**Rejected Reasons:**

1. **Silent permanent hang**: `ChildStdin` consumed by panic, cannot be cloned, restart creates zombie writer
2. **Resource leak**: Original stdin handle lost, new writer cannot write
3. **Debugging nightmare**: Appears to work but silently fails
4. **Better alternative exists**: Respawn entire connection with fresh stdin (~100-500ms)

## Related Decisions

- **[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)**: Server Pool Coordination
  - Relies on ls-bridge-message-ordering's ConnectionState for router integration
- **[ls-bridge-async-connection](ls-bridge-async-connection.md)**: Async Bridge Connection
  - ls-bridge-message-ordering builds on tokio runtime, uses ChildStdin from process spawning
- **[ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md)**: Graceful Shutdown
  - Defines behavior for Closing/Closed states in the ConnectionState enum
- **[language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md)**: Virtual document model
  - Stable URIs (PBI-200) enable consistent request tracking

## References

**Design Pattern Origins**: The thin bridge pattern follows the end-to-end principle—don't add complexity in the middle layer for something the endpoints already handle. LSP clients manage stale requests via `$/cancelRequest`; servers handle concurrent requests efficiently.

## Future Considerations

### Phase 2: Optional Bridge-Level Coalescing

If profiling shows excessive load from rapid-fire requests (e.g., user typing very quickly), add optional coalescing:

**Proposed Mechanism:**

```rust
struct CoalescingMap {
    // Key: (URI, method) → Value: (generation, request_id, envelope)
    map: HashMap<(Uri, Method), (u64, RequestId, Envelope)>,
}
```

- Each (URI, method) key has a monotonic generation counter
- New request supersedes old → old gets immediate `REQUEST_CANCELLED`
- Writer loop uses atomic claim pattern to detect superseded requests

**When to Enable:**
- Per-server configuration (some servers may benefit more than others)
- Or adaptive: enable when pending requests exceed threshold

**Trade-offs:**
- **Pro**: Reduced server load for rapid-fire requests
- **Pro**: Immediate `REQUEST_CANCELLED` feedback
- **Con**: Additional complexity (coalescing map, generation counters)
- **Con**: Bridge makes assumptions about what's "stale"

**Deferred because**: Most servers handle concurrent requests efficiently; client `$/cancelRequest` provides adequate stale request management.

### Request Queuing During Initialization

The current design rejects requests with `REQUEST_FAILED` during initialization. A future enhancement could queue requests and drain them after `didOpen`.

**Trade-offs:**
- **Pro**: No user-visible errors during initialization
- **Pro**: First hover/completion works immediately after server ready
- **Con**: Queue management complexity (memory bounds, timeouts)
- **Con**: Stale requests may be processed (user moved cursor during init)

**Deferred because**: Current design prioritizes simplicity and transparency; client retry behavior provides acceptable UX.

## Amendment History

- **2026-01-06**: Merged Amendment 001 - Completed state machine with all transitions, panic handler implementation requirements, and error code corrections
- **2026-01-06**: Merged Amendment 002 - Added comprehensive notification drop telemetry and state re-synchronization metadata to prevent silent data loss
- **2026-08-12**: Applied the contract/invariant/mechanism discipline (template.md) - deleted the allocation arithmetic, the inbound response-slot reservation pool, and the lock-private `Flushing` phase, and added an Invariants section recording the ordering traps they closed. The admission threshold and the pre-ready hold's admissibility rules are unchanged. Two wire-visible rules were lost in that first pass and restored on review: that admission stays closed while the combined depth sits above the threshold, and that the flush phase is not an observable connection state
