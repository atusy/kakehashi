# ADR-0020: Pull-First Diagnostic Forwarding Strategy

| | |
|---|---|
| **Status** | Accepted (Phases 1-3 Implemented) |
| **Date** | 2026-01-29 |

**Related**:
- [ADR-0008](0008-language-server-bridge-request-strategies.md): Request-Specific Bridge Strategies (Strategy 4: Background Collection)
- [ADR-0016](0016-ls-bridge-server-pool-coordination.md): Server Pool Coordination (Notification Forwarding)
- [ADR-0015](0015-ls-bridge-message-ordering.md): Message Ordering

## Context

kakehashi acts as an LSP bridge between editors and downstream language servers for injection regions (e.g., Lua code blocks in Markdown). Diagnostic forwarding is critical for providing error feedback to users editing embedded code.

### The Diagnostic Forwarding Challenge

Diagnostics must be:
1. **Aggregated**: Multiple injection regions in one host document produce diagnostics from multiple virtual documents
2. **Transformed**: Virtual document positions must map back to host document positions
3. **Fresh**: Diagnostics must reflect the current document state, not stale versions
4. **Timely**: Users expect feedback within a reasonable latency window

### Original Approach: Push-First (ADR-0008 Strategy 4)

The original plan followed a push-first model:

```
downstream LS ──publishDiagnostics──> kakehashi ──transform & aggregate──> client
```

This approach requires:
- **Notification interception**: Capture `publishDiagnostics` from downstream servers
- **InjectionContextTracker**: Maintain reverse mapping from `region_id` to host context
- **Diagnostic cache**: Store per-region diagnostics for aggregation
- **Lifecycle management**: Clear cache on document close, region invalidation, server crash
- **Race condition handling**: Version tracking to discard stale diagnostics
- **Complex state synchronization**: Keep `InjectionContextTracker` in sync with edits

### The Fundamental Insight

The push model requires kakehashi to maintain complex state that mirrors information already available at request time. Every `publishDiagnostics` notification arrives asynchronously, requiring careful coordination with document lifecycle events.

**LSP 3.17 introduced pull diagnostics** (`textDocument/diagnostic`) specifically to address these challenges. In the pull model, the client initiates diagnostic requests, giving the server (kakehashi) complete control over timing and state.

## Decision

**Adopt a pull-first diagnostic forwarding strategy**, where kakehashi responds to diagnostic requests by fan-out querying downstream servers, then aggregating results synchronously.

### Architecture Overview

```
Phase 1: Pure Pull
┌────────────────────────────────────────────────────────────────────────┐
│                                                                        │
│  Client ──textDocument/diagnostic──► kakehashi                         │
│                                          │                             │
│                    ┌─────────────────────┼─────────────────────┐       │
│                    ▼                     ▼                     ▼       │
│              lua-ls (pull)        pyright (pull)        taplo (pull)   │
│                    │                     │                     │       │
│                    └─────────────────────┼─────────────────────┘       │
│                                          ▼                             │
│                                    Aggregate & Transform               │
│                                          │                             │
│  Client ◄──DiagnosticResponse────────────┘                             │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘

Phase 2-3: Synthetic Push (using pull internally)
┌────────────────────────────────────────────────────────────────────────┐
│                                                                        │
│  Client ──didSave/didOpen/didChange──► kakehashi                       │
│                                            │                           │
│                             (internally pull from downstream servers)  │
│                                            │                           │
│  Client ◄──publishDiagnostics──────────────┘                           │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘

Phase 6: Cancellation-Resilient Pull (Optional)
┌────────────────────────────────────────────────────────────────────────┐
│                                                                        │
│  Client ──textDocument/diagnostic──► kakehashi                         │
│                                          │                             │
│                              (fan-out to downstream servers)           │
│                                          │                             │
│  Client ──$/cancelRequest────────────────┼──► (continue in background) │
│                                          │                             │
│                              (downstream responses arrive)             │
│                                          │                             │
│                              (check: document still valid?)            │
│                                          │                             │
│  Client ◄──publishDiagnostics────────────┘                             │
│                                                                        │
└────────────────────────────────────────────────────────────────────────┘
```

### Phased Implementation

#### Phase 1: Pure Pull Diagnostic

kakehashi implements `textDocument/diagnostic` handler:

1. Receive pull diagnostic request from client
2. Identify all injection regions in the host document
3. Fan-out `textDocument/diagnostic` requests to downstream servers
4. Wait for all responses (with timeout)
5. Transform positions: virtual to host
6. Aggregate diagnostics from all regions
7. Return combined response

**Key simplification**: No caching, no lifecycle tracking, no race conditions. The diagnostic state exists only for the duration of the request.

#### Phase 2: Synthetic Push on didSave/didOpen

Extend Phase 1 to proactively publish diagnostics:

1. On `didSave` or `didOpen`, kakehashi internally performs Phase 1 logic
2. Publishes aggregated diagnostics via `textDocument/publishDiagnostics`

This provides push-like UX for clients that don't support pull diagnostics, while using pull internally.

#### Phase 3: Higher Frequency Diagnostics (Implemented)

Extend triggers for diagnostic refresh:

- Debounced `didChange` (500ms after last change) - **Implemented**
- Idle detection (e.g., 2s of no activity) - Deferred

Same internal mechanism: pull from downstream, aggregate, publish.

**Implementation Notes:**
- `DebouncedDiagnosticsManager` tracks per-document debounce timers
- Snapshot data is captured at schedule time for consistency
- Timer cancellation on document close or new changes
- Shares `SyntheticDiagnosticsManager` for task superseding
- Debounce duration: 500ms (configurable via `DEFAULT_DEBOUNCE_DURATION`)

#### Phase 4: Legacy Server Support

For downstream servers that don't support `textDocument/diagnostic`:

- Cache their `publishDiagnostics` notifications
- When kakehashi needs to respond to a pull request:
  - For pull-capable servers: query via `textDocument/diagnostic`
  - For push-only servers: use cached diagnostics

Caching in this phase is **required for correctness** — without it, legacy servers cannot participate in diagnostics. Phase 5 adds optional caching for performance optimization.

#### Phase 5: Full Reactive Optimization

- Cache recent pull results to avoid redundant queries
- Implement `workspace/diagnostic/refresh` to invalidate caches
- React to any downstream `publishDiagnostics` to update cache and publish

#### Phase 6: Cancellation-Resilient Pull (Optional)

Handle client-side cancellation gracefully when bridge overhead causes slow responses:

1. On pull diagnostic request cancellation, continue processing in background
2. When downstream responses arrive, check if the host document is still valid (not stale)
3. If valid, publish aggregated diagnostics via `textDocument/publishDiagnostics`
4. If stale (document has changed), discard results

**Rationale**: This phase is optional because:
- Users can simply re-request diagnostics if a request is cancelled
- Phases 2-3 (synthetic push) provide proactive diagnostics, reducing reliance on pull requests
- The added complexity of background task management may not be worth the benefit

**When to implement**: Consider this phase only if users frequently experience cancellations due to bridge latency and synthetic push (Phases 2-3) is insufficient for the use case.

### Position Transformation

Position transformation remains the same regardless of push or pull:

```
Virtual Position (UTF-16)
         │
         ▼
   position_to_byte()     [Virtual UTF-16 -> Virtual byte]
         │
         ▼
   + content_start_byte   [Virtual byte -> Host byte]
         │
         ▼
   byte_to_position()     [Host byte -> Host UTF-16]
         │
         ▼
Host Position (UTF-16)
```

This transformation is performed synchronously during request handling, using information available from the injection resolver.

### Aggregation Strategy

For each host document, diagnostics are aggregated by:

1. Collecting diagnostics from all injection regions
2. Transforming each diagnostic's range to host coordinates
3. Transforming `relatedInformation` locations if present
4. Merging all diagnostics into a single array
5. Deduplicating by (range, message, severity) if needed

## Consequences

### Positive

**Dramatic Simplification:**
- No `InjectionContextTracker` required (Phases 1-3)
- No diagnostic cache required (Phases 1-3)
- No lifecycle event handling for diagnostics
- No race condition mitigation for version mismatches
- Position transformation uses existing injection resolver data

**Request-Scoped State:**
- All necessary context exists at request time
- No asynchronous state synchronization
- Easier to reason about, test, and debug

**LSP Protocol Alignment:**
- Pull diagnostics is the modern LSP 3.17 approach
- Designed specifically for scenarios like kakehashi's aggregation needs
- Growing editor support (VS Code, Neovim 0.10+, Helix, Zed)

**Graceful Degradation:**
- Clients without pull support get synthetic push (Phases 2-3)
- Servers without pull support get cached push (Phase 4)

### Negative

**Server Compatibility:**
- Not all downstream servers support `textDocument/diagnostic`
- Phase 4 adds caching complexity for legacy servers
- Some servers (older versions) may have incomplete pull support

**Latency Characteristics:**
- Pull requires round-trip to downstream servers at request time
- First diagnostic response may be slower than pre-computed push
- Mitigated by Phases 2-3 proactive publishing

**Client Compatibility:**
- Older editors may not support pull diagnostics
- Mitigated by Phases 2-3 synthetic push

### Neutral

**Same Position Transformation:**
- Byte-based position mapping remains identical
- No change to coordinate transformation complexity

**Same Aggregation Logic:**
- Multi-region aggregation is the same regardless of push vs pull
- Just happens synchronously instead of asynchronously

## Alternatives Considered

### Alternative 1: Push-First with Full State Tracking

Implement diagnostics via `publishDiagnostics` interception with:
- `InjectionContextTracker` for reverse mapping
- Diagnostic cache per (host_uri, server_name, region_id)
- Lifecycle event handlers for cleanup
- Version tracking for staleness detection

**Not Chosen Because:**
- Significantly more complex state management
- Race conditions between notifications and edits
- Must keep `InjectionContextTracker` synchronized with document changes
- Debugging asynchronous state bugs is difficult
- Duplicates information available from injection resolver

### Alternative 2: Hybrid First-Class Push with Pull Fallback

Start with push model, add pull support later.

**Not Chosen Because:**
- Inverts the complexity gradient: harder path first
- Pull is simpler to implement and extend
- Modern LSP direction favors pull

### Alternative 3: Client-Side Aggregation

Forward raw `publishDiagnostics` from virtual URIs, let clients aggregate.

**Not Chosen Because:**
- Exposes internal virtual document URIs to clients
- Clients don't understand kakehashi's injection model
- Violates abstraction: host document should appear as single entity

## Server Support Considerations

| Server | Pull Support | Notes |
|--------|--------------|-------|
| rust-analyzer | Yes | Supports `textDocument/diagnostic` |
| pyright | Yes | Full pull diagnostic support |
| lua-language-server | Yes | Recent versions support pull |
| typescript-language-server | Yes | Via tsserver |
| gopls | Yes | Full pull support |
| taplo | Unknown | May require Phase 4 (Legacy Server Support) fallback |

For servers without pull support, Phase 4 (Legacy Server Support) provides the cache-based fallback.

## Implementation Notes

### Fan-Out Timeout

When querying multiple downstream servers:
- Use per-request timeout (e.g., 5 seconds)
- Return partial results if some servers timeout
- Mark response as potentially incomplete

### Error Handling

```
For each injection region:
  Try: Query downstream server for diagnostics
  On success: Transform and collect
  On timeout: Log warning, continue with other regions
  On error: Log error, continue with other regions

Return: Aggregated diagnostics from successful queries
```

### Capability Advertisement

kakehashi should advertise pull diagnostic support:

```json
{
  "capabilities": {
    "diagnosticProvider": {
      "interFileDependencies": false,
      "workspaceDiagnostics": false
    }
  }
}
```

## Related ADRs

- **[ADR-0007](0007-language-server-bridge-virtual-document-model.md)**: Virtual Document Model
  - Defines how injection regions become virtual documents
- **[ADR-0008](0008-language-server-bridge-request-strategies.md)**: Request Strategies
  - This ADR supersedes Strategy 4 (Background Collection) for diagnostics
- **[ADR-0015](0015-ls-bridge-message-ordering.md)**: Message Ordering
  - Request forwarding patterns apply to diagnostic fan-out
- **[ADR-0016](0016-ls-bridge-server-pool-coordination.md)**: Server Pool Coordination
  - Notification Forwarding section updated by this decision
