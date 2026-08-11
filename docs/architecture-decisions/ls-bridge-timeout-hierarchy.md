# LS Bridge Timeout Hierarchy

**Related Decisions**:
- [ls-bridge-async-connection](ls-bridge-async-connection.md) § Liveness Timeout & Initialization Timeout
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) § Response Aggregation
- [ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md) § Shutdown Timeout

**Phasing**: See ls-bridge-implementation-phasing — Phase 1 (Init, Liveness, Global Shutdown), Phase 3 (Per-Request).

## Scope

This decision coordinates timeout mechanisms across the bridge architecture. It defines:
- Timeout tier hierarchy and precedence rules
- Interaction semantics when multiple timeouts are active
- State transitions triggered by each timeout

**Phase 1 Timeouts** (implemented now): Initialization (Tier 0), Liveness (Tier 2), Global Shutdown (Tier 3)

**Phase 3 Timeout** (future): Per-Request (Tier 1) — only needed for multi-server aggregation

## Context

The async bridge architecture defines timeout systems across several decisions:

1. **Initialization Timeout** (ls-bridge-async-connection): Bounds server initialization time during startup
2. **Liveness Timeout** (ls-bridge-async-connection): Detects hung servers (unresponsive to pending requests)
3. **Global Shutdown Timeout** (ls-bridge-graceful-shutdown): Bounds the shutdown termination attempt (escalation); ownership disposition and local cleanup run as actor transitions outside the ceiling
4. **Per-Request Timeout** (ls-bridge-server-pool-coordination): Bounds user-facing latency for multi-server aggregation *[Phase 3 only]*
5. **Per-Slot Control Shutdown Timeout** (bridge-client-control-protocol): Bounds a single-slot `stop`/`restart` (see the dedicated section below)
6. **Routing Decision Deadline** (bridge-routing-protocol): Bounds one routing decision — provider fan-out plus initialization waits (see the dedicated section below)

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
| **2** | Liveness | 30-120s | Ready state + liveness-classified managed pending > 0 (pass-through and routing queries excluded via the same per-entry classification — bridge-client-control-protocol, bridge-routing-protocol; today every pending entry counts) | `Ready` → `Failed` (pool may spawn replacement) |
| **3** | Global Shutdown | 5-15s | Shutdown initiated | SIGTERM → SIGKILL, all → `Closed` |

**State-Based Gating:**
- **Initialization timeout**: Only during `Initializing` state; disabled on shutdown
- **Liveness timeout**: Only during `Ready` state with pending requests; disabled on shutdown
- **Global shutdown**: Overrides all other timeouts (highest priority), including an in-flight per-slot control shutdown deadline (subsumed by `Teardown`)

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
| Shutdown (any state) | Global only | All other timeouts (Init/Liveness/Per-request) STOP; an in-flight per-slot control shutdown is subsumed by the `Teardown` transition, its deadline superseded by the global one; global bounds the termination attempt |
| Late response during shutdown | Global | ACCEPT until the connection closes or the deadline expires |

**Key Interactions:**
- Liveness timeout **STOPS** when entering `Closing` state
- Initialization timeout **CANCELLED** on shutdown (global takes over)
- Per-request timeout **STOPS** on shutdown (futures receive `RecvError` from closed channels)
- Late responses accepted until the connection closes or the applicable deadline expires (server is responsive, not hung)

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
- Single ceiling for the termination attempt during pool teardown (not per-server; local cleanup falls outside)
- Graceful attempts → SIGTERM → SIGKILL escalation
- Reserve ~20% of timeout for SIGTERM/SIGKILL (e.g., 10s total → 8s graceful + 2s forced)

**Per-Slot Control Shutdown** (bridge-client-control-protocol):
- A single-slot `stop`/`restart` runs under its own per-connection shutdown
  timeout with the same graceful → SIGTERM → SIGKILL shape
- **Duration**: implementation-defined, default in the same 5-15s class
  as the global ceiling; one deadline covers queue drain, handshake, and
  escalation initiation — a child unconfirmed beyond it converts to a
  termination-pending record rather than extending the deadline
- This is not the rejected per-server teardown timeout: it bounds one
  user-initiated control operation while the rest of the pool keeps
  serving; pool teardown keeps the single global ceiling above
- Precedence on overlap: `Teardown` is a message on the same
  lifecycle-actor queue, so it is ordered against every in-flight control
  transition by construction; from the teardown transition on, the global
  deadline governs, and the escalation reserve force-kills whatever a
  per-slot operation has not finished (ls-bridge-graceful-shutdown
  § Lifecycle Actor)

**Routing Decision Deadline** (bridge-routing-protocol):
- One deadline bounds an entire routing decision: the concurrent provider
  fan-out and any bounded initialization waits inside it
- **Duration**: implementation-defined, documented default in the
  low-seconds class
- On expiry the pending provider requests are cancelled
  (`$/cancelRequest`) and their entries retired atomically with the
  fallback answer; the decision falls open to kakehashi-decided routing
- Routing requests are excluded from Tier-2 liveness accounting (same
  per-entry classification as pass-through): they carry their own
  deadline, and a slow provider must never drive a `Ready` connection to
  `Failed`
- Also exempt from the Tier-1 per-request timeout (whose fan-out trigger
  would otherwise cover a routing fan-out once Phase 3 lands): the
  routing deadline is the sole bound on these requests
- Global teardown overrides: decisions waiting on provider handshakes
  resolve to the fallback immediately

**Writer-Idle Timeout** (within the applicable shutdown deadline):
- **Duration**: 2s fixed
- **Purpose**: Wait for writer loop to finish current operation before taking exclusive stdin access
- **Scope**: Counts against the applicable shutdown budget — per-slot `stop` or global teardown — not additional time
- **See**: ls-bridge-graceful-shutdown § Writer Loop Shutdown Synchronization

## Consequences

### Positive

- **Deterministic behavior**: Clear precedence when multiple timeouts could fire
- **Bounded shutdown**: Global timeout bounds the termination attempt; a child unconfirmed at the deadline is retained or abandoned per ls-bridge-graceful-shutdown § Unconfirmed termination
- **Hung server detection**: Liveness timeout catches unresponsive servers

### Negative

- **Multiple concepts**: Three timeout *tiers* in Phase 1 (four in Phase 3), plus the tier-exempt deadlines registered here (per-slot control shutdown, routing decision, writer-idle)
- **Tuning required**: Implementation-defined values need careful selection

### Neutral

- **LSP compliant**: Request timeouts trigger explicit error responses, not silent hangs (the routing decision deadline is the exception by design — it falls open to kakehashi-decided routing with a warning, never an upstream error)

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

## Related Decisions

- **[ls-bridge-async-connection](ls-bridge-async-connection.md)**: Defines liveness and initialization timeouts
- **[ls-bridge-message-ordering](ls-bridge-message-ordering.md)**: Connection state machine (state-based timeout gating)
- **[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)**: Per-request timeout *(Phase 3)*
- **[ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md)**: Global shutdown timeout
- **[bridge-routing-protocol](bridge-routing-protocol.md)**: Routing decision deadline; Tier-1/Tier-2 exemptions for routing queries

## Summary

**Phase 1**: Three timeout tiers — Initialization (30-60s), Liveness (30-120s), Global Shutdown (5-15s)

**Phase 3**: Adds Per-Request timeout (2-5s) for multi-server aggregation

**Key Rule**: Global shutdown overrides all other timeouts
