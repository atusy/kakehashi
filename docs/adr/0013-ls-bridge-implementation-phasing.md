# ADR-0013: LS Bridge Implementation Phasing

| | |
|---|---|
| **Date** | 2026-01-08 |
| **Status** | Draft |
| **Type** | Cross-ADR Coordination |

## Context

The async bridge architecture spans multiple ADRs (0014-0018), each defining features at different complexity levels. Without a clear phasing strategy, it's unclear:
- What must be implemented first
- What can be deferred
- How features depend on each other

### Design Principle

**Minimal and Extensible**: Start with the simplest working system, extend incrementally.

## Decision

**Define three implementation phases with clear boundaries and dependencies.**

## Phase Definitions

### Phase 1: Single-LS-per-Language (Current Target)

**Goal**: One downstream language server per language, simple routing, fail-fast error handling.

| Component | Phase 1 Behavior |
|-----------|------------------|
| **Routing** | `languageId` → single server (ADR-0016) |
| **Requests during init** | `REQUEST_FAILED` immediately (ADR-0015) |
| **Cancellation** | Forward `$/cancelRequest` to downstream (ADR-0015) |
| **Timeouts** | Init, Liveness, Global Shutdown (ADR-0018) |
| **Coalescing** | None — trust client/server (ADR-0015) |

**What Works:**
- Multiple embedded languages (Python + Lua + TOML in markdown)
- Parallel server initialization
- Per-downstream document lifecycle
- Graceful shutdown with global timeout

**What Doesn't Work Yet:**
- Multiple servers for same language (pyright + ruff for Python)
- Response aggregation/merging
- Rate-limited respawn (respawn storms possible)

### Phase 2: Resilience Patterns (Future)

**Goal**: Add crash resilience without changing routing model.

| Component | Phase 2 Addition |
|-----------|------------------|
| **Rate-limited respawn** | Max N respawns per time window, backoff on limit |
| **Telemetry** | `$/telemetry` events for crashes/backoff |
| **Coalescing** | Optional, profile-driven (ADR-0015 Future) |

**Why Rate-limited Respawn**: Language servers fail by crashing, not by returning errors while alive. Rate-limited respawn prevents respawn storms when a server keeps crashing (e.g., bad config, missing dependency).

**Rate-Limited Respawn Specification**:

*Mechanism*:
- Sliding window rate limiting per language
- Exponential backoff with cap when limit exceeded
- Self-healing: retries indefinitely, no permanent death state

*Behavior*:
- Requests during backoff receive `REQUEST_FAILED` ("server recovering")
- Telemetry: fire-and-forget, never blocks respawn decisions

*Shutdown*: Respawn suppressed when pool is shutting down (per ADR-0015 § 6).

*Parameters (max respawns, window size, backoff intervals) are implementation-defined.*

**Dependencies**: Phase 1 complete.

### Phase 3: Multi-LS-per-Language (Future)

**Goal**: Multiple servers for the same language with response aggregation.

| Component | Phase 3 Addition |
|-----------|------------------|
| **Routing** | Fan-out to multiple servers (ADR-0016) |
| **Aggregation** | preferred, concatenated, deduplicated, … strategies |
| **Per-Request Timeout** | Bounds aggregation latency (ADR-0018) |
| **Backpressure** | Multi-server coordination (ADR-0016) |

**Use Case**: pyright (types) + ruff (linting) for Python simultaneously.

**Dependencies**: Phase 1 complete. Phase 2 recommended for resilience.

## Phase Feature Matrix

| Feature | Phase 1 | Phase 2 | Phase 3 |
|---------|:-------:|:-------:|:-------:|
| Multiple languages | ✅ | ✅ | ✅ |
| Simple routing (lang→server) | ✅ | ✅ | ✅ |
| Parallel initialization | ✅ | ✅ | ✅ |
| Graceful shutdown | ✅ | ✅ | ✅ |
| Fail-fast during init | ✅ | ✅ | ✅ |
| Cancellation forwarding | ✅ | ✅ | ✅ |
| Rate-limited respawn | ❌ | ✅ | ✅ |
| Telemetry | ❌ | ✅ | ✅ |
| Coalescing (optional) | ❌ | ✅ | ✅ |
| Multiple servers/language | ❌ | ❌ | ✅ |
| Response aggregation | ❌ | ❌ | ✅ |
| Per-request timeout | ❌ | ❌ | ✅ |

## ADR Phase Mapping

| ADR | Phase 1 Scope | Phase 2 Additions | Phase 3 Additions |
|-----|---------------|-------------------|-------------------|
| **0014** (Connection) | Async I/O, timeouts | — | — |
| **0015** (Ordering) | Thin bridge, forwarding | Optional coalescing, telemetry | — |
| **0016** (Coordination) | Simple routing, lifecycle | Rate-limited respawn | Aggregation, fan-out |
| **0017** (Shutdown) | Graceful shutdown | — | — |
| **0018** (Timeouts) | Init, Liveness, Global | — | Per-request timeout |

## Consequences

### Positive

- **Clear implementation order**: Know what to build first
- **Reduced complexity**: Each ADR can focus on current phase
- **Testable milestones**: Phase 1 is a complete, working system

### Negative

- **Feature limitations**: Phase 1 can't do pyright + ruff simultaneously
- **Documentation overhead**: Must track which phase each feature belongs to

### Neutral

- **Extensibility preserved**: Phase 2/3 features documented but not blocking

## Related ADRs

- **[ADR-0014](0014-ls-bridge-async-connection.md)**: Async Bridge Connection
- **[ADR-0015](0015-ls-bridge-message-ordering.md)**: Message Ordering
- **[ADR-0016](0016-ls-bridge-server-pool-coordination.md)**: Server Pool Coordination
- **[ADR-0017](0017-ls-bridge-graceful-shutdown.md)**: Graceful Shutdown
- **[ADR-0018](0018-ls-bridge-timeout-hierarchy.md)**: Timeout Hierarchy
