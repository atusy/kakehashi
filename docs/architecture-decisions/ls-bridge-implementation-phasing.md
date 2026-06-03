# LS Bridge Implementation Phasing

| | |
|---|---|
| **Date** | 2026-01-08 |
| **Status** | Draft |
| **Type** | Cross-Decision Coordination |

## Context

The async bridge architecture spans multiple decisions (0014-0018), each defining features at different complexity levels. Without a clear phasing strategy, it's unclear:
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
| **Routing** | `languageId` → single server (ls-bridge-server-pool-coordination) |
| **Requests during init** | `REQUEST_FAILED` immediately (ls-bridge-message-ordering) |
| **Cancellation** | Forward `$/cancelRequest` to downstream (ls-bridge-message-ordering) |
| **Timeouts** | Init, Liveness, Global Shutdown (ls-bridge-timeout-hierarchy) |
| **Coalescing** | None — trust client/server (ls-bridge-message-ordering) |

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
| **Coalescing** | Optional, profile-driven (ls-bridge-message-ordering Future) |

**Why Rate-limited Respawn**: Language servers fail by crashing, not by returning errors while alive. Rate-limited respawn prevents respawn storms when a server keeps crashing (e.g., bad config, missing dependency).

**Rate-Limited Respawn Specification**:

*Mechanism*:
- Sliding window rate limiting per language
- Exponential backoff with cap when limit exceeded
- Self-healing: retries indefinitely, no permanent death state

*Behavior*:
- Requests during backoff receive `REQUEST_FAILED` ("server recovering")
- Telemetry: fire-and-forget, never blocks respawn decisions

*Shutdown*: Respawn suppressed when pool is shutting down (per ls-bridge-message-ordering § 6).

*Parameters (max respawns, window size, backoff intervals) are implementation-defined.*

**Dependencies**: Phase 1 complete.

### Phase 3: Multi-LS-per-Language (Future)

**Goal**: Multiple servers for the same language with response aggregation.

| Component | Phase 3 Addition |
|-----------|------------------|
| **Routing** | Fan-out to multiple servers (ls-bridge-server-pool-coordination) |
| **Aggregation** | preferred, concatenated, deduplicated, … strategies |
| **Per-Request Timeout** | Bounds aggregation latency (ls-bridge-timeout-hierarchy) |
| **Backpressure** | Multi-server coordination (ls-bridge-server-pool-coordination) |

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

## Decision Phase Mapping

| decision | Phase 1 Scope | Phase 2 Additions | Phase 3 Additions |
|-----|---------------|-------------------|-------------------|
| **0014** (Connection) | Async I/O, timeouts | — | — |
| **0015** (Ordering) | Thin bridge, forwarding | Optional coalescing, telemetry | — |
| **0016** (Coordination) | Simple routing, lifecycle | Rate-limited respawn | Aggregation, fan-out |
| **0017** (Shutdown) | Graceful shutdown | — | — |
| **0018** (Timeouts) | Init, Liveness, Global | — | Per-request timeout |

## Consequences

### Positive

- **Clear implementation order**: Know what to build first
- **Reduced complexity**: Each decision can focus on current phase
- **Testable milestones**: Phase 1 is a complete, working system

### Negative

- **Feature limitations**: Phase 1 can't do pyright + ruff simultaneously
- **Documentation overhead**: Must track which phase each feature belongs to

### Neutral

- **Extensibility preserved**: Phase 2/3 features documented but not blocking

## Related Decisions

- **[ls-bridge-async-connection](ls-bridge-async-connection.md)**: Async Bridge Connection
- **[ls-bridge-message-ordering](ls-bridge-message-ordering.md)**: Message Ordering
- **[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)**: Server Pool Coordination
- **[ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md)**: Graceful Shutdown
- **[ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md)**: Timeout Hierarchy
