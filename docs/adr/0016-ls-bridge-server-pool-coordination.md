# ADR-0016: LS Bridge Server Pool Coordination

| | |
|---|---|
| **Status** | Draft |
| **Date** | 2026-01-07 |

**Related**:
- [ADR-0014](0014-ls-bridge-async-connection.md): Single-connection async I/O
- [ADR-0015](0015-ls-bridge-message-ordering.md): Single-server message ordering

**Phasing**: See [ADR-0013](0013-ls-bridge-implementation-phasing.md) — Phase 1 (routing), Phase 2 (rate-limited respawn), Phase 3 (aggregation).

## Scope

This ADR defines how to coordinate **multiple downstream language server connections** from a single bridge. It covers:
- Server pool lifecycle (spawn, initialize, shutdown)
- Request routing to appropriate server(s)
- Document lifecycle per downstream server
- Notification handling (drop, forward, pass-through)

## Context

The bridge manages connections to multiple downstream language servers. Even in Phase 1, multiple servers exist (e.g., pyright for Python, lua-ls for Lua). Each connection follows ADR-0014 (async I/O) and ADR-0015 (message ordering).

### Key Challenges

1. **Lifecycle Management**: How do we spawn, initialize, and shut down multiple servers?
2. **Request Routing**: Given a request for a language, which server should receive it?
3. **Document State**: How do we track document lifecycle per downstream server?
4. **Partial Failures**: What happens when some servers initialize successfully but others fail?

## Decision

**Adopt a phased approach: start with single-LS-per-language routing, extend to multi-LS aggregation in Phase 3.**

### Phase 1: Server-Name-Based Routing (Current)

Each language maps to a `server_name` via configuration. The pool is keyed by `server_name`, enabling:
- **Process sharing for related languages**: e.g., `typescript` and `typescriptreact` can share a single `tsgo` process
- **Decoupling language from process**: The same binary can serve multiple languages

Routing: `language` → `server_name` (via config) → connection.

**No-Provider Handling:** Return `REQUEST_FAILED` with clear message ("bridge: no provider for hover in python") to keep misconfiguration visible.

## Architecture

### Server Pool Architecture (Phase 1)

```
┌─────────────────────────────────────────────────────────┐
│                   kakehashi (Host LS)                   │
│  ┌────────────────────────────────────────────────────┐ │
│  │              LanguageServerPool                    │ │
│  │                                                    │ │
│  │   ┌─────────────────┐                              │ │
│  │   │  RequestRouter  │ ── routes by languageId      │ │
│  │   └────────┬────────┘                              │ │
│  │            │                                       │ │
│  │            │    (Phase 1: one server per language) │ │
│  │            ▼                                       │ │
│  │ ┌───────────┐  ┌───────────┐  ┌───────────┐        │ │
│  │ │  pyright  │  │  lua-ls   │  │  taplo    │        │ │
│  │ │ (python)  │  │  (lua)    │  │  (toml)   │        │ │
│  │ └───────────┘  └───────────┘  └───────────┘        │ │
│  │      ↑              ↑              ↑               │ │
│  │      └──────────────┴──────────────┘               │ │
│  │           Each: ADR-0014 + ADR-0015                │ │
│  └────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

**Phase 1 Design:**
- **RequestRouter**: Routes by `languageId` to single server
- **Per-Connection Isolation**: Each downstream connection maintains its own actor (ADR-0015)
- **No Aggregation**: Single server per language, no fan-out

**Phase 3 Extension**: ResponseAggregator for multi-LS fan-out (see Future Extensions).

### Request ID Semantics

**Decision**: Use upstream request IDs directly for downstream servers.

**Phase 1 Flow** (single server per language):
```
Client (editor)          kakehashi           Downstream Server
     ├─ hover ID=42 ────→ Router ──────────────→ pyright (ID=42)
     ◀─ result ─────────────────────────────────◀
```

**Benefits:**
- Request ID consistent across client → bridge → server
- Simple state management (one pending entry per request)
- No ID transformation needed

### Routing (Phase 1)

In Phase 1, routing resolves `languageId` → `server_name` via configuration, then looks up the connection by `server_name`:

```rust
// Phase 1: Server-name-based routing
fn route_request(language_id: &str) -> Option<&Connection> {
    // 1. Resolve server_name from config (e.g., "typescript" → "tsgo")
    let server_name = config.get_server_for_language(language_id)?;
    // 2. Look up connection by server_name (enables process sharing)
    self.connections.get(server_name)
}
```

**Key difference from pure language-based routing**: Multiple languages can share the same connection when they map to the same `server_name`. For example, `typescript` and `typescriptreact` can both resolve to `tsgo`, sharing a single process.

**Phase 3 Extension**: Multi-LS routing strategies (SingleByCapability, FanOut) — see Future Extensions.

### Server Lifecycle Management

**Parallel Initialization:**

Multiple downstream servers initialize in parallel since each is independent:

```
┌─────────┐     ┌──────────┐     ┌──────────┐
│ Bridge  │     │ pyright  │     │  lua-ls  │
└────┬────┘     └────┬─────┘     └────┬─────┘
     │──initialize──▶│                │
     │──initialize───────────────────▶│  (parallel, no wait)
     │◀──result──────│                │  (pyright responds first)
     │──initialized─▶│                │
     │──didOpen─────▶│                │  (pyright ready for Python)
     │◀──result───────────────────────│  (lua-ls responds later)
     │──initialized──────────────────▶│
     │──didOpen──────────────────────▶│  (lua-ls ready for Lua)
```

**Key Points:**
- **Parallel `initialize`**: Send to all servers concurrently
- **Independent lifecycle**: Each server proceeds as soon as it responds
- **No global barrier**: Fast servers start handling requests immediately

**Partial Initialization Failure:**

| Scenario | Behavior |
|----------|----------|
| All servers succeed | Normal operation |
| Some servers fail | Continue with working servers, respawn failed |
| All servers fail | Bridge reports errors, continues respawning |

**Future Extension (Phase 2)**: Rate-limited respawn to prevent respawn storms.

**Per-Downstream Document Lifecycle:**

Maintain the latest host-document snapshot per downstream. When a slower server reaches `didOpen`, send the full text as of "now", not as of when the first downstream opened.

**Document Lifecycle States** (per downstream, per URI):

```
States: Opened | Closed

Default: Closed (absent entry = Closed)

Transitions:
- Closed → Opened         (didOpen sent to downstream)
- Opened → Closed         (didClose sent to downstream)
```

**Why `Closed` as default**: From the downstream server's perspective, "never opened" and "was opened, now closed" are functionally equivalent—both require `didOpen` before any document operations. Using `Closed` as the default simplifies re-opening: it's just the normal `Closed → Opened` transition.

**Notification Handling by State:**

| Notification | Closed State (default) | Opened State |
|--------------|------------------------|--------------|
| `didOpen` | **SEND**, transition to **Opened** | Unexpected (log warning) |
| `didChange` | **DROP** (didOpen contains current state) | **FORWARD** |
| `didSave` | **DROP** | **FORWARD** |
| `willSave` | **DROP** | **FORWARD** |
| `didClose` | Suppress (already closed) | **FORWARD**, transition to **Closed** |

**Why drop instead of queue**: The `didOpen` notification contains the complete document text at send time. Accumulated client edits are included. Dropping `didChange` before `didOpen` avoids duplicate state updates.

**Connection Termination**: When a connection enters `Closed` state (graceful shutdown, crash, or respawn), all document lifecycle entries for that downstream are discarded. A respawned connection starts with all documents in `Closed` (default) state, requiring fresh `didOpen` notifications.

### Server-to-Client Notification Forwarding

Server-initiated notifications from downstream servers are forwarded to the upstream client with optional modifications.

```
downstream ──notification──►  bridge  ──notification──►  upstream
                               │
                               ├─ Transform uri and positions (virtual -> host)
                               ├─ Transform content for distinguishability
                               │    (e.g., to prefix title with downstream server name)
                               ├─ Aggregate for multi-injection regions
                               │    (e.g., to show diagnostics for multiple code blocks in markdown)
                               └─ ...
```

### Cancellation Propagation

See [ADR-0015 § Cancellation Forwarding](0015-ls-bridge-message-ordering.md#5-cancellation-forwarding) for single-connection cancellation semantics.

**Multi-Connection Coordination (Phase 3)**: Router forwards `$/cancelRequest` to all connections that received the original fan-out request.

## Future Extensions

### Phase 3: Multi-Server Backpressure Coordination

When routing notifications to multiple servers for the same language (Phase 3), if one server's queue is full, notifications are handled independently per server.

**Decision**: Accept state divergence under extreme backpressure (non-atomic broadcast).

```
Router sends didSave to pyright + ruff (both handle Python):
├─ pyright: queue full → DROP (per ADR-0015)
└─ ruff: queue OK → FORWARD

Result: State divergence (recoverable via next didChange)
```

**Rationale**: Servers already handle being attached at arbitrary points in a document's lifetime.

### Phase 3: Response Aggregation Strategies

> **Note**: This section describes Phase 3 multi-LS-per-language features. Phase 1 uses single-server routing.

For fan-out **requests** (with `id`), configure aggregation per method:

```rust
enum AggregationStrategy {
    /// Return first successful response, cancel others.
    /// On first success: immediately send $/cancelRequest to remaining servers,
    /// discard any late responses.
    FirstWins,

    /// Wait for all, merge array results (candidate lists only)
    MergeAll {
        dedup_key: Option<String>,  // e.g., 'label' for completions
        max_items: Option<usize>,
    },

    /// Wait for all, return highest priority non-null result
    Ranked {
        priority: Vec<String>,
    },
}
```

**Aggregation Stability Rules:**
- **Per-request timeout conditions**: Timeout applies **only when n ≥ 2 downstream servers participate**
  - SingleByCapability: No per-request timeout (wait for single server, liveness timeout protects)
  - FanOut with n=1: No per-request timeout (functionally equivalent to single)
  - FanOut with n≥2: Per-request timeout applies (default: 5s explicit, 2s incremental)
- **Per-request timeout behavior**: On timeout, return whatever results available **without sending $/cancelRequest**
  - Downstream servers continue processing and send responses
  - Late responses **discarded** by router but **reset liveness timeout** (heartbeat for connection health)
  - **Memory management**: Request entry removed from `pending_responses` after returning partial results
- **Partial results**: If at least one downstream succeeds, respond with successful `result` using LSP-native fields (e.g., for CompletionList: `{ "isIncomplete": true, "items": [...] }`)
- **Total failure**: If all downstreams fail or time out, respond with `ResponseError` (`REQUEST_FAILED`)

**Aggregation Error Messages:**

| Scenario | Error Code | Message |
|----------|------------|---------|
| All servers timeout, no responses | `REQUEST_FAILED` | "bridge: aggregation timeout, no responses received" |
| All servers return errors | `REQUEST_FAILED` | "bridge: all downstream servers failed" |
| No servers configured for method | `REQUEST_FAILED` | "bridge: no provider for {method} in {language}" |

### Phase 3: Configuration Example

```yaml
# Phase 3: Multiple servers per language with aggregation
languages:
  markdown:
    bridges:
      python:
        # Multiple servers for Python
        priority: ["ruff", "pyright"]  # Prioritize ruff when capability overlaps

        # Per-method aggregation config:
        aggregations:
          textDocument/completion:
            strategy: concatenated   # Safe: candidates, user selects one
            dedup_key: label
          textDocument/codeAction:
            strategy: concatenated   # Safe: proposals, user executes one
          # hover, definition: use default (single_by_capability)
          # formatting, rename: MUST use single_by_capability

languageServers:
  pyright:
    cmd: [pyright-langserver, --stdio]
    languages: [python]
  ruff:
    cmd: [ruff, server]
    languages: [python]  # Same language as pyright
```

## Consequences

### Positive

**Simple Routing (Phase 1):**
- Language → single server mapping is straightforward
- No aggregation overhead for common cases

**Graceful Degradation:**
- Partial initialization failures allow working servers to continue
- Fault isolation: One crashed server doesn't affect others

**Parallel Initialization:**
- Multiple servers initialize concurrently without global barriers
- Faster servers start handling requests immediately

**No Silent Failures:**
- Missing providers surface as explicit `REQUEST_FAILED` errors
- Users can diagnose configuration issues immediately

**Extensible Foundation:**
- Phase 1 architecture supports future multi-LS extension (Phase 3)
- Single-server configurations continue to work unchanged

### Negative

**Single Server Limitation (Phase 1):**
- Cannot use multiple servers for same language (e.g., pyright + ruff) until Phase 3

**Coordination Complexity:**
- Per-downstream document state tracking required
- State divergence possible under extreme backpressure

### Neutral

**Existing Tests:**
- Current single-server tests remain valid

**Diagnostics:**
- Pass-through by design — client handles aggregation

**Phase 3 Trade-offs** (future):
- Aggregation adds complexity and latency
- Configuration surface grows with multi-LS support

## Alternatives Considered

### Alternative 1: Sequential Initialization

Initialize servers one at a time, waiting for each to complete.

**Rejected Reasons:**

1. **Increased startup time**: N servers × init time = long wait
2. **No benefit**: Server initialization is independent, parallelization is free
3. **Poor UX**: Users wait for slowest server before any work

**Why parallel is better**: Faster servers can start handling requests immediately.

### Alternative 2: Global Initialization Barrier

Wait for ALL servers to initialize before handling any requests.

**Rejected Reasons:**

1. **Slow server blocks all**: One slow server delays entire system
2. **No partial utility**: Fast servers sit idle waiting
3. **Fragile**: One failure delays everything

**Why per-server independence is better**: Each language proceeds as soon as its server is ready.

### Alternative 3: Drop Notifications Silently Before didOpen

Silently discard notifications instead of explicit DROP with state tracking.

**Rejected Reasons:**

1. **Hidden behavior**: Hard to debug why notifications don't reach server
2. **No state visibility**: Can't tell if notification was dropped or queued
3. **Inconsistent**: Some notifications reach server, others don't

**Why explicit state is better**: Clear rules for notification handling based on document lifecycle state.

## Configuration Example (Phase 1)

```yaml
# Phase 1: Server-name-based routing with process sharing
languages:
  markdown:
    bridges:
      python:
        server: pyright          # Single server for Python
      lua:
        server: lua-ls           # Single server for Lua
      typescript:
        server: tsgo             # TypeScript → tsgo
      typescriptreact:
        server: tsgo             # TSX → same tsgo (process sharing!)
      toml:
        server: taplo            # Single server for TOML

languageServers:
  pyright:
    cmd: [pyright-langserver, --stdio]
    languages: [python]
  lua-ls:
    cmd: [lua-language-server, --stdio]
    languages: [lua]
  tsgo:
    cmd: [tsgo, --stdio]
    languages: [typescript, typescriptreact]  # Serves both ts and tsx
  taplo:
    cmd: [taplo, lsp, stdio]
    languages: [toml]
```

**Process Sharing Example**: In the above configuration, `typescript` and `typescriptreact` both map to `server: tsgo`. When a request comes for either language:
1. Configuration resolves the language to `server_name: "tsgo"`
2. Pool looks up connection by `"tsgo"` (not by language)
3. Both languages share the same process, improving resource usage

**Phase 3 Configuration Example** (future): See Future Extensions for multi-LS aggregation config.

## Related ADRs

- **[ADR-0006](0006-language-server-bridge.md)**: Core LSP bridge architecture (1:1 pattern)
  - ADR-0016 extends to 1:N (one client → multiple servers per language)
- **[ADR-0008](0008-language-server-bridge-request-strategies.md)**: Per-method bridge strategies
  - Per-method strategies remain valid for single-server routing
- **[ADR-0012](0012-multi-ls-async-bridge-architecture.md)**: Multi-LS async bridge **(Parent ADR)**
  - This ADR extracts multi-server coordination from ADR-0012
- **[ADR-0014](0014-ls-bridge-async-connection.md)**: Async Bridge Connection (single-server I/O)
  - Provides async I/O patterns enabling parallel server management
- **[ADR-0015](0015-ls-bridge-message-ordering.md)**: Message Ordering
  - Handles single-server ordering; ADR-0016 coordinates multiple servers
- **[ADR-0017](0017-ls-bridge-graceful-shutdown.md)**: Graceful Shutdown
  - Defines shutdown coordination for multiple concurrent connections
  - Router broadcasts shutdown; ADR-0017 specifies per-connection sequence

## Amendment History

- **2026-01-24**: Changed from language-based to server-name-based pool keying to enable process sharing for related languages (e.g., ts/tsx sharing tsgo). Connection pool is now keyed by `server_name` instead of `languageId`, with configuration resolving `language` → `server_name`.
- **2026-01-07**: Merged [Amendment 002](0015-multi-server-coordination-amendment-002.md) - Simplified ID namespace by using upstream request IDs directly (no transformation), replaced `pending_correlations` with `pending_responses`
- **2026-01-06**: Merged [Amendment 001](0015-multi-server-coordination-amendment-001.md) - Updated partial results to use LSP-native fields (isIncomplete), clarified $/cancelRequest semantics, added response guarantees for cancelled requests
