# ADR-0015: LS Bridge Message Ordering

| | |
|---|---|
| **Status** | Accepted |
| **Date** | 2026-01-06 |

**Supersedes**:
- [ADR-0012](0012-multi-ls-async-bridge-architecture.md) § Timeout-based control
- [ADR-0009](0009-async-bridge-architecture.md): Original async architecture

**Phasing**: See [ADR-0013](0013-ls-bridge-implementation-phasing.md) — This ADR covers Phase 1; optional coalescing deferred to Phase 2.

## Scope

This ADR defines message ordering guarantees for **a single connection** to a downstream language server. It covers:
- Single-writer actor loop for protocol correctness
- Connection state machine (Initializing → Ready → Failed/Closing → Closed)
- Operation gating based on connection state
- Cancellation forwarding to downstream servers

**Out of Scope**: Coordination of multiple connections (routing, aggregation) is covered by ADR-0016.

## Context

### Problems with Previous Approach

ADR-0012 established timeout-based control for initialization and request superseding. This approach had three fundamental problems:

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
│  │  Bounded capacity (256)                          │    │
│  │  - Ensures FIFO ordering                         │    │
│  │  - Non-blocking backpressure (try_send)          │    │
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

Requests are forwarded directly to downstream servers without coalescing or superseding.

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

The bounded order queue (capacity: 256) uses `try_send()` to prevent deadlocks during slow initialization.

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
    Closing,       // Shutdown initiated, draining operations (ADR-0017)
    Closed,        // Connection fully terminated
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
| `Initializing` | success (init response received) | `Ready` |
| `Initializing` | timeout / failure / crash / panic | `Failed` |
| `Initializing` | shutdown signal | `Closing` |
| `Ready` | shutdown signal | `Closing` |
| `Ready` | crash / panic | `Failed` |
| `Failed` | (automatic) | `Closed` |
| `Closing` | (graceful or timeout) | `Closed` |
| `Closing` | panic | `Closed` |

**Key Transition: Failed → Closed (Direct)**

`Failed` transitions directly to `Closed`, bypassing `Closing`. This is because:
- `Failed` state means the connection is already broken (panic, crash, timeout)
- LSP shutdown handshake is impossible (stdin may be unavailable)
- ADR-0017 specifies: `Failed → Closed` with process cleanup only (SIGTERM → SIGKILL)

**Operation Gating:**

Operations are gated at two levels: **server lifecycle** and **document lifecycle**.

**Server Lifecycle Gating** (ConnectionState):
- Writer loop starts immediately in `Initializing` state
- **Requests**: Gated on `Ready` state:
  - `Initializing` → `REQUEST_FAILED` ("bridge: downstream server initializing")
  - `Failed` → `REQUEST_FAILED` ("bridge: downstream server failed")
  - `Closing` → `REQUEST_FAILED` ("bridge: connection closing") [See ADR-0017]
  - `Closed` → `REQUEST_FAILED` ("bridge: connection closed")
- **Notifications**: Accepted by writer loop in `Initializing` or `Ready` state only
  - `Closing`/`Closed` → DROP (writer loop stopped, see ADR-0017)
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
- Forward `$/cancelRequest` notification to downstream server
- Keep pending request entry (response still expected)
- Forward whatever response the server sends (result or REQUEST_CANCELLED error)

**Rationale**: The bridge doesn't need to intercept cancellation. Downstream servers implement `$/cancelRequest` per LSP spec—they either:
- Complete the request (too late to cancel) → forward result
- Cancel successfully → forward REQUEST_CANCELLED error

**Coordination with ADR-0016:** Router forwards `$/cancelRequest` to all connections that received the original request.

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
- During shutdown: `Failed → Closed` (see ADR-0017), no respawn

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
    line = reader.read_line() => { /* handle response */ }
    _ = token.cancelled() => { break; }  // Writer panicked, exit
    _ = shutdown_rx.recv() => { break; } // Graceful shutdown
}
```

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
- Notifications can be dropped if queue full (256+ operations)
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

## Related ADRs

- **[ADR-0012](0012-multi-ls-async-bridge-architecture.md)**: Multi-LS async bridge architecture
  - ADR-0015 supersedes timeout-based control while maintaining LSP compliance
- **[ADR-0016](0016-ls-bridge-server-pool-coordination.md)**: Server Pool Coordination
  - Relies on ADR-0015's ConnectionState for router integration
- **[ADR-0014](0014-ls-bridge-async-connection.md)**: Async Bridge Connection
  - ADR-0015 builds on tokio runtime, uses ChildStdin from process spawning
- **[ADR-0017](0017-ls-bridge-graceful-shutdown.md)**: Graceful Shutdown
  - Defines behavior for Closing/Closed states in the ConnectionState enum
- **[ADR-0007](0007-language-server-bridge-virtual-document-model.md)**: Virtual document model
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

- **2026-01-06**: Merged [Amendment 001](0014-actor-based-message-ordering-amendment-001.md) - Completed state machine with all transitions, panic handler implementation requirements, and error code corrections
- **2026-01-06**: Merged [Amendment 002](0014-actor-based-message-ordering-amendment-002.md) - Added comprehensive notification drop telemetry and state re-synchronization metadata to prevent silent data loss
