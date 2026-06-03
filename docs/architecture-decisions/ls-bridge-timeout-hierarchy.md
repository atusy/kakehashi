# ADR-0018: LS Bridge Timeout Hierarchy

| | |
|---|---|
| **Date** | 2026-01-06 |
| **Status** | Draft |
| **Type** | Cross-ADR Coordination |

**Related ADRs**:
- [ADR-0014](0014-ls-bridge-async-connection.md) § Liveness Timeout & Initialization Timeout
- [ADR-0016](0016-ls-bridge-server-pool-coordination.md) § Response Aggregation
- [ADR-0017](0017-ls-bridge-graceful-shutdown.md) § Shutdown Timeout

**Phasing**: See [ADR-0013](0013-ls-bridge-implementation-phasing.md) — Phase 1 (Init, Liveness, Global Shutdown), Phase 3 (Per-Request).

## Scope

This ADR coordinates timeout mechanisms across the bridge architecture. It defines:
- Timeout tier hierarchy and precedence rules
- Interaction semantics when multiple timeouts are active
- State transitions triggered by each timeout

**Phase 1 Timeouts** (implemented now): Initialization (Tier 0), Liveness (Tier 2), Global Shutdown (Tier 3)

**Phase 3 Timeout** (future): Per-Request (Tier 1) — only needed for multi-server aggregation

## Context

The async bridge architecture defines timeout systems across three ADRs:

1. **Initialization Timeout** (ADR-0014): Bounds server initialization time during startup
2. **Liveness Timeout** (ADR-0014): Detects hung servers (unresponsive to pending requests)
3. **Global Shutdown Timeout** (ADR-0017): Bounds total shutdown time
4. **Per-Request Timeout** (ADR-0016): Bounds user-facing latency for multi-server aggregation *[Phase 3 only]*

### The Problem

Without clear precedence rules, timeout interactions are non-deterministic:
- What happens if shutdown starts during initialization?
- Should liveness timeout fire during shutdown?
- Which timeout triggers state transitions?

## Decision

**Establish a three-tier timeout hierarchy for Phase 1** (four tiers in Phase 3).

### Phase 1 Timeout Tiers

| Tier | Timeout | Duration | Trigger | Action |
|------|---------|----------|---------|--------|
| **0** | Initialization | 30-60s | `initialize` request sent | `Initializing` → `Failed` (pool may spawn replacement) |
| **2** | Liveness | 30-120s | Ready state + pending > 0 | `Ready` → `Failed` (pool may spawn replacement) |
| **3** | Global Shutdown | 5-15s | Shutdown initiated | SIGTERM → SIGKILL, all → `Closed` |

**State-Based Gating:**
- **Initialization timeout**: Only during `Initializing` state; disabled on shutdown
- **Liveness timeout**: Only during `Ready` state with pending requests; disabled on shutdown
- **Global shutdown**: Overrides all other timeouts (highest priority)

### Phase 3 Addition: Per-Request Timeout (Tier 1)

> **Note**: Only needed for multi-server aggregation. In Phase 1, liveness timeout provides sufficient protection.

| Tier | Timeout | Duration | Trigger | Action |
|------|---------|----------|---------|--------|
| **1** | Per-Request | 2-5s | Fan-out to n≥2 servers | Return partial results or `REQUEST_FAILED` |

### Precedence Rules

**Global shutdown overrides all other timeouts.**

| Scenario | Active Timeouts | Behavior |
|----------|----------------|----------|
| Normal operation (Phase 1) | Liveness | Reset on activity; `Ready` → `Failed` on timeout |
| Normal operation (Phase 3) | Liveness, Per-request | Per-request bounds aggregation; Liveness detects hung servers |
| Shutdown (any state) | Global only | All other timeouts (Init/Liveness/Per-request) STOP; global enforces termination |
| Late response during shutdown | Global | ACCEPT until global timeout expires |

**Key Interactions:**
- Liveness timeout **STOPS** when entering `Closing` state
- Initialization timeout **CANCELLED** on shutdown (global takes over)
- Per-request timeout **STOPS** on shutdown (futures receive `RecvError` from closed channels)
- Late responses accepted until global timeout (server is responsive, not hung)

## Configuration Recommendations

| Timeout | Recommended | Rationale |
|---------|-------------|-----------|
| **Initialization** | 30-60s | Heavy servers (rust-analyzer) need time to index |
| **Liveness** | 30-120s | Detect hung servers without false positives |
| **Global Shutdown** | 5-15s | Balance clean exit vs user wait time |
| **Per-Request** *(Phase 3)* | 2-5s | User-facing latency bound for aggregation |

**Relationships:**
```
Initialization (60s) > Liveness (30-120s) > Per-request (5s)
Global Shutdown overrides all (highest priority)
```

**Global Shutdown Design:**
- Single ceiling for entire shutdown (not per-server)
- Graceful attempts → SIGTERM → SIGKILL escalation
- Reserve ~20% of timeout for SIGTERM/SIGKILL (e.g., 10s total → 8s graceful + 2s forced)

**Writer-Idle Timeout** (within Global Shutdown):
- **Duration**: 2s fixed
- **Purpose**: Wait for writer loop to finish current operation before taking exclusive stdin access
- **Scope**: Counts against global shutdown budget (not additional time)
- **See**: ADR-0017 § Writer Loop Shutdown Synchronization

## Consequences

### Positive

- **Deterministic behavior**: Clear precedence when multiple timeouts could fire
- **Bounded shutdown**: Global timeout guarantees termination
- **Hung server detection**: Liveness timeout catches unresponsive servers

### Negative

- **Multiple concepts**: Three timeout systems in Phase 1 (four in Phase 3)
- **Tuning required**: Implementation-defined values need careful selection

### Neutral

- **LSP compliant**: Timeouts trigger explicit error responses, not silent hangs

## Alternatives Considered

### Alternative 1: Single Global Timeout

Use one timeout for everything.

**Rejected**: Conflicting requirements (init needs 60s, user actions need 2-5s).

### Alternative 2: Per-Server Timeouts

Each server has independent timeout that can multiply.

**Rejected**: Unbounded total time (N servers × timeout = too slow for shutdown).

### Alternative 3: Implicit Precedence

Let implementation details determine which timeout wins.

**Rejected**: Non-deterministic, hard to debug, race conditions.

## Related ADRs

- **[ADR-0014](0014-ls-bridge-async-connection.md)**: Defines liveness and initialization timeouts
- **[ADR-0015](0015-ls-bridge-message-ordering.md)**: Connection state machine (state-based timeout gating)
- **[ADR-0016](0016-ls-bridge-server-pool-coordination.md)**: Per-request timeout *(Phase 3)*
- **[ADR-0017](0017-ls-bridge-graceful-shutdown.md)**: Global shutdown timeout

## Summary

**Phase 1**: Three timeout tiers — Initialization (30-60s), Liveness (30-120s), Global Shutdown (5-15s)

**Phase 3**: Adds Per-Request timeout (2-5s) for multi-server aggregation

**Key Rule**: Global shutdown overrides all other timeouts
