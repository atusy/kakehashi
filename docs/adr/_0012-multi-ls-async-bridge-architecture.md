# ADR-0012: Multi-Language Server Async Bridge Architecture

## Status

Deprecated (2026-01-05)

**Superseded by**:
- [ADR-0014: Actor-Based Message Ordering](0014-actor-based-message-ordering.md) — Message ordering with generation-based coalescing (replaces timeout-based control)
- [ADR-0015: Multi-Server Coordination](0015-multi-server-coordination.md) — Multi-server routing and lifecycle management

**Previously superseded**:
- [ADR-0009](0009-async-bridge-architecture.md): Single-LS async architecture (**completely replaced** due to unfixable hang issues)
- [ADR-0008](0008-language-server-bridge-request-strategies.md): Per-method strategies (**partially replaced** for multi-LS aspects; single-LS strategies remain valid)

---

## Deprecation Notice

**Reason for deprecation:**
This ADR attempted to solve both message ordering and multi-server coordination in a single architecture. The timeout-based control approach proved insufficient for handling initialization windows and notification ordering. The new ADRs adopt an **event-driven actor pattern** with generation-based coalescing that provides stronger ordering guarantees without timeout complexity.

**Migration path:**
- Message ordering/superseding → See [ADR-0014](0014-actor-based-message-ordering.md)
- Multi-server routing/lifecycle → See [ADR-0015](0015-multi-server-coordination.md)
- Async I/O infrastructure → See [ADR-0013](0013-async-io-layer.md)

---

## Context

### Current Problems

ADR-0009 established the tokio-based async bridge architecture for concurrent LSP request handling with a single downstream language server. However, the current implementation suffers from critical issues:

**1. Severe Hang Issues**
- Async tasks occasionally hang indefinitely waiting for responses
- Root cause: Complex interaction between tokio wakers and channel notification timing
- Multiple fix attempts (yield_now, mpsc channels, Notify) provide partial relief but don't eliminate hangs

**2. LSP Ordering Violations**
- Notifications and requests can arrive out of order at downstream servers
- Can violate LSP spec requirement: `didOpen` must precede other document notifications
- Current mutex-based serialization insufficient for multi-document scenarios

**3. Limited to Single LS per Language**
- No support for multiple servers handling same language (e.g., pyright + ruff for Python)
- No aggregation or routing strategies for overlapping capabilities
- Cannot leverage complementary strengths of different servers

### Real-World Requirements

Real-world usage requires bridging to **multiple downstream language servers** simultaneously:
- Python code in markdown may need both **pyright** (type checking, completion) and **ruff** (linting, formatting)
- Embedded SQL may need both a SQL language server and the host language server
- Future polyglot scenarios (e.g., TypeScript + CSS in Vue files)

## Decision

**Re-implement from scratch** with simpler, proven patterns. The new architecture uses a **routing-first, aggregation-optional** approach that supports 1:N communication patterns.

### Core Requirements

1. **LSP Compliance** — All error handling uses standard LSP error codes and response structures
2. **Fan-out/Scatter-Gather** — Send requests to multiple LSes, aggregate responses when needed
3. **Ordering Guarantees** — Notifications maintain order per (downstream, document)
4. **Cancellation Propagation** — `$/cancelRequest` from upstream flows to all downstream
5. **Resilience** — Circuit breaker and bulkhead patterns for fault isolation

## Architecture

### 1. LSP Error Codes and Response Structures

All error responses use standard LSP error codes (LSP 3.17+) to maintain protocol compliance:

```rust
/// LSP-compliant error codes
pub struct ErrorCodes;

impl ErrorCodes {
    /// Request failed but was syntactically correct (LSP 3.17)
    /// Use for: downstream server failures, timeouts, circuit breaker open
    pub const REQUEST_FAILED: i32 = -32803;

    /// Server cancelled the request (LSP 3.17)
    /// Only for requests that explicitly support server cancellation
    pub const SERVER_CANCELLED: i32 = -32802;

    /// Server not initialized (JSON-RPC reserved)
    /// Use for: requests/notifications sent before `initialized`
    pub const SERVER_NOT_INITIALIZED: i32 = -32002;
}

/// LSP-compliant error response structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
```

**Critical LSP requirement (LSP 3.x § Response Message):** Every request MUST receive a response. Never return `None`, drop requests, or leave them hanging. Use `ResponseError` with appropriate error codes for all failure scenarios.

**Usage guidelines:**
- Use `REQUEST_FAILED` (-32803) for most error scenarios: timeouts, downstream failures, circuit breaker open
- Include human-readable `message` describing the specific error context
- Optional `data` field can provide additional debug information (e.g., which downstream server failed)

### 2. Design Principle: Routing First

**Most requests should be routed to a single downstream LS based on capabilities.** Aggregation is only needed when multiple LSes provide overlapping functionality that must be combined.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                           Request Routing                                │
│                                                                          │
│   Incoming Request                                                       │
│         │                                                                │
│         ▼                                                                │
│   ┌─────────────────────────────────────────────────────────────────┐    │
│   │ Which LSes have this capability for this languageId?            │    │
│   └─────────────────────────────────────────────────────────────────┘    │
│         │                                                                │
│         ├── 0 LSes → Return REQUEST_FAILED (-32803) w/ “no provider”     │
│         ├── 1 LS   → Route to single LS (no aggregation needed)          │
│         └── N LSes → Check routing strategy:                             │
│                        ├── SingleByCapability → Pick alphabetically first│
│                        └── FanOut → Send to all, aggregate responses     │
└──────────────────────────────────────────────────────────────────────────┘

**Priority order:** Users can explicitly define a `priority` list in the bridge configuration. If not defined, the bridge falls back to a deterministic order based on server names (sorted alphabetically).
  - Explicit: `priority: ["ruff", "pyright"]` → `ruff` is checked first.
  - Default: `pyright` vs `ruff` → `pyright` wins (alphabetical).

**No-provider handling:** Returning `REQUEST_FAILED` with a clear message (“no downstream language server provides hover for python”) keeps misconfiguration visible instead of silently returning `null`.
```

**Example: pyright + ruff for Python**

| Method | pyright | ruff | Routing Strategy |
|--------|---------|------|------------------|
| `hover` | ✅ | ❌ | → pyright only (no aggregation) |
| `definition` | ✅ | ❌ | → pyright only |
| `completion` | ✅ | ✅ | → FanOut + concatenated |
| `formatting` | ❌ | ✅ | → ruff only |
| `codeAction` | ✅ | ✅ | → FanOut + concatenated |
| `diagnostics` | ✅ | ✅ | → Both (notification pass-through, no aggregation) |

### 3. System Architecture

**Naming decision:** The existing `TokioAsyncLanguageServerPool` and `TokioAsyncBridgeConnection` mix domain with implementation technique. Since we're doing a complete rewrite, we'll use clean domain names:
- **`LanguageServerPool`** (was `TokioAsyncLanguageServerPool`) - manages language server connections
- **`BridgeConnection`** (was `TokioAsyncBridgeConnection`) - represents a single downstream server connection
- **Rationale**: Implementation techniques (tokio, async) are internal details that shouldn't leak into class names. If we ever change async runtimes or patterns, names remain accurate. Implementation techniques belong in module documentation, not class names.

**Implementation approach:** The new `LanguageServerPool` will be a complete rewrite addressing hang issues while adding multi-LS support. The old `TokioAsyncLanguageServerPool` and `TokioAsyncBridgeConnection` will be replaced.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                         kakehashi (Host LS)                         │
│  ┌────────────────────────────────────────────────────────────────────┐ │
│  │                      LanguageServerPool                            │ │
│  │                                                                    │ │
│  │   ┌─────────────────┐                                              │ │
│  │   │  RequestRouter  │ ─── routes by (method, languageId, caps)     │ │
│  │   └────────┬────────┘                                              │ │
│  │            │                                                       │ │
│  │   ┌────────┴────────┐    Fan-out: scatter to multiple LSes         │ │
│  │   │                 │                                              │ │
│  │   ▼                 ▼                                              │ │
│  │ ┌───────────┐  ┌───────────┐  ┌───────────┐                        │ │
│  │ │  pyright  │  │   ruff    │  │ lua-ls    │  ... per-LS connection │ │
│  │ │(conn + Q) │  │(conn + Q) │  │(conn + Q) │                        │ │
│  │ └─────┬─────┘  └─────┬─────┘  └─────┬─────┘                        │ │
│  │       │              │              │                              │ │
│  │   ┌───┴──────────────┴──────────────┴───┐                          │ │
│  │   │         ResponseAggregator          │  Fan-in: merge/rank      │ │
│  │   └─────────────────────────────────────┘                          │ │
│  └────────────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────────────┘
```

**Key Design Points:**
- **Clean Separation**: Domain name (`LanguageServerPool`) independent of implementation technique (tokio/async)
- **Complete Rewrite**: New implementation from scratch with simpler patterns to eliminate hang issues
- **New Components**: `RequestRouter` and `ResponseAggregator` are new components not present in the current implementation
- **ID Namespace Isolation**: Each downstream connection maintains its own `next_request_id`. The pool maps `(upstream_id, downstream_key)` → `downstream_id` for correlation.
- **Per-Connection Send Queue**: Each connection serializes writes via `Mutex<ChildStdin>`, ensuring no byte-level corruption.
- **Aggregation Strategies**: Configurable per method (see section 8).

### 4. Routing Strategies

```rust
enum RoutingStrategy {
    /// Route to single LS with highest priority (default)
    /// No aggregation needed - fast path
    SingleByCapability {
        priority: Vec<String>,  // e.g., ["pyright", "ruff"]
    },

    /// Fan-out to multiple LSes, aggregate responses
    /// Only for methods where overlapping results must be combined
    FanOut {
        aggregation: AggregationStrategy,
    },
}
```

### 5. Lifecycle and Initialization

#### 5.1 Initialization Protocol

**LSP mandates that `didOpen` and other notifications must be sent AFTER `initialized` notification (LSP 3.x § Server lifecycle):**

```
initialize (request)      →  Server
                          ←  initialize (response)
initialized (notification)→  Server
────────────────────────────────────────
didOpen (notification)    →  Server     ← NOW allowed
hover (request)           →  Server     ← NOW allowed
```

#### 5.2 Parallel Multi-Server Initialization

When connecting to multiple downstream language servers, `initialize` requests can be sent in parallel since each server is an independent process with no inter-server dependencies.

```
┌─────────┐     ┌──────────┐     ┌──────────┐
│ Bridge  │     │ pyright  │     │   ruff   │
└────┬────┘     └────┬─────┘     └────┬─────┘
     │               │                │
     │──initialize──▶│                │
     │──initialize───────────────────▶│  (parallel, no wait)
     │               │                │
     │◀──result──────│                │  (pyright responds first)
     │──initialized─▶│                │
     │──didOpen─────▶│                │  (pyright ready, can use)
     │               │                │
     │◀──result───────────────────────│  (ruff responds later)
     │──initialized──────────────────▶│
     │──didOpen──────────────────────▶│  (ruff now ready)
```

Key points:
- **Parallel `initialize`**: Send to all servers concurrently without waiting
- **Independent lifecycle**: Each server's `initialized` → `didOpen` proceeds as soon as that server responds
- **No global barrier**: Servers that initialize faster can start handling requests immediately

#### 5.3 Partial Initialization Failure Policy

When initializing multiple downstream servers in parallel, some may succeed while others fail. The bridge handles this gracefully:

| Scenario | Behavior | Rationale |
|----------|----------|-----------|
| All servers initialize successfully | Normal operation with all servers | Expected case |
| Some servers fail initialization | Continue with working servers, failed servers enter circuit breaker open state | Graceful degradation - pyright failures shouldn't block ruff usage |
| All servers fail initialization | Bridge reports errors but remains alive, circuit breakers prevent request routing | Allow recovery without full bridge restart |

**Error propagation:**
- Failed `initialize` requests trigger circuit breaker for that specific server
- Requests routed to failed servers receive `REQUEST_FAILED` with circuit breaker message
- Client sees degraded functionality (e.g., "pyright unavailable") rather than total failure
- **Fan-out awareness:** If a method is configured for aggregation (e.g., completion concatenated) and one server is in circuit breaker/open or still uninitialized, the router skips it and proceeds with the available servers. The aggregator marks the response as partial in `data` so UX continues instead of blocking on an unhealthy peer.

### 6. Notification Handling

#### 6.1 Two-Phase Notification Handling During Initialization

Notifications must navigate two critical phases before entering normal operation:

| Phase | Duration | Notification Handling |
|-------|----------|----------------------|
| **Phase 1: Before `initialized`** | spawn → `initialized` sent | Block all notifications except `initialized` itself with `SERVER_NOT_INITIALIZED` error |
| **Phase 2: Before `didOpen`** | `initialized` sent → `didOpen` sent | Document notifications (`didChange`, `didSave`) are not forwarded immediately. Their state changes are aggregated into the document version sent with `didOpen`. Other notifications proceed. |
| **Normal Operation** | After `didOpen` sent | All notifications forwarded normally |

**Rationale for dropping document notifications in Phase 2:**

During the initialization window (after `initialized` but before kakehashi sends `didOpen` to downstream), the client may send `didChange` notifications to kakehashi for the host document. These must be translated to the virtual document for downstream, but:

1. **State accumulation**: Any changes before `didOpen` are already incorporated into the document state that will be sent
2. **LSP semantics**: The LSP spec mandates `didOpen` must precede `didChange` for the same document (LSP 3.x § Text Document Synchronization)
3. **Bridge control**: kakehashi controls when to send `didOpen` downstream, thus also when to start forwarding `didChange`

**Example scenario:**
```
┌────────┐     ┌──────────────┐     ┌──────────┐
│ Client │     │ kakehashi│     │ pyright  │
└───┬────┘     └──────┬───────┘     └────┬─────┘
    │──didOpen(md)───▶│                  │
    │                 │ (spawn pyright)  │
    │──didChange(md)─▶│ ❌ DROP          │  ← Client edits during init
    │                 │ (pyright initializing...)
    │                 │◀──init result────│
    │                 │──initialized────▶│
    │                 │──didOpen(virt)──▶│  ← Includes ALL changes
    │                 │                  │
    │──didChange(md)─▶│──didChange(virt)▶│  ← NOW forward normally
```

**Per-downstream snapshotting:** Maintain the latest host-document snapshot (or aggregated pending edits) per downstream. When a slower server reaches its `didOpen`, send the full text as of “now”, not as of when the first downstream opened. Queue any later `didChange` for that downstream until its `didOpen` is sent, so every server starts from an accurate, synchronized state. If the host sends `didClose` before a downstream receives `didOpen`, mark that downstream as “closed” and suppress the pending `didOpen` entirely (do **not** create a ghost open). If `didOpen` was already sent and `didClose` arrived during initialization, queue the `didClose` and flush it immediately after initialization completes to preserve open/close pairing.

**Implementation:**

```rust
pub async fn send_notification(&self, method: &str, params: Value) -> Result<(), ResponseError> {
    // Phase 1 guard: block all notifications before initialized (except "initialized" itself)
    if !self.initialized.load(Ordering::SeqCst) && method != "initialized" {
        return Err(ResponseError {
            code: ErrorCodes::SERVER_NOT_INITIALIZED,
            message: "Cannot send notification: downstream language server not initialized".to_string(),
            data: None,
        });
    }

    // Phase 2 guard: handle notifications before didOpen sent (PER-DOCUMENT)
    if let Some(uri) = extract_uri_from_params(&params, method) {
        let has_opened = {
            let opened = self.opened_documents.lock().await;
            opened.contains(&uri)
        };

        if !has_opened {
            match method {
                "initialized" => {
                    // Always allow - this sets the initialized flag
                }
                "textDocument/didOpen" => {
                    // Send and mark that THIS DOCUMENT has been opened
                    // ... send logic ...
                    {
                        let mut opened = self.opened_documents.lock().await;
                        opened.insert(uri.to_string());
                    }
                    return Ok(());
                }
                "textDocument/didChange" => {
                    // Accumulate change - will be included in didOpen content
                    if let Some(changes) = params.get("contentChanges") {
                        let mut pending = self.pending_changes.lock().await;
                        pending.entry(uri.clone())
                            .or_insert_with(Vec::new)
                            .extend(parse_changes(changes));
                    }
                    log::debug!("Accumulated {} for {}; state will be in didOpen", method, uri);
                    return Ok(());
                }
                "textDocument/didSave" | "textDocument/didClose" => {
                    // Do not forward - document not opened yet
                    log::debug!("Not forwarding {} for {} during init", method, uri);
                    return Ok(());
                }
                _ => {
                    // Other notifications proceed after initialized
                }
            }
        }
    }

    // ... normal send logic ...
}
```

Phase 1/2 guards are internal only. Notifications must not emit responses on the wire; upstream observes silence while the bridge logs or traces the dropped/blocked notification.

**State tracking:**

```rust
struct BridgeConnection {
    // ... existing fields ...
    initialized: AtomicBool,         // true after "initialized" notification sent
    initialized_notify: Notify,      // wake tasks waiting for initialization

    // CRITICAL: Per-document tracking (NOT connection-level)
    opened_documents: Arc<Mutex<HashSet<String>>>,  // Track which virtual docs have didOpen sent
    pending_changes: Arc<Mutex<HashMap<String, Vec<ContentChange>>>>,  // Accumulate changes before didOpen
}
```

**IMPORTANT - Per-Document vs Connection-Level Tracking:**

The Phase 2 guard MUST track `didOpen` status **per virtual document**, NOT per connection. A single bridge connection may handle multiple virtual documents (e.g., multiple Python code blocks in the same markdown file), and each must track its own lifecycle independently.

**Anti-pattern (WRONG):**
```rust
// ❌ Connection-level flag - breaks with multiple documents
if !self.did_open_sent.load(Ordering::SeqCst) {
    // Drops didChange for ALL documents if ANY hasn't opened yet
}
```

**Correct pattern:**
```rust
// ✅ Per-document tracking
if let Some(uri) = extract_uri_from_params(&params, method) {
    let has_opened = {
        let opened = self.opened_documents.lock().await;
        opened.contains(&uri)
    };

    if !has_opened {
        // Only drop for THIS specific document
    }
}
```

**Queue prioritization to avoid head-of-line blocking:** Per-connection send queues prioritize text synchronization (`didOpen`/`didChange`/`didClose`) ahead of long-running requests, preventing large requests from delaying document state updates. For finer isolation in the future, move to per-document queues within a connection while preserving in-order delivery per document.

#### 6.2 Document Notification Order

**Problem:**
```
upstream: didChange(v10) → completion
If completion reaches downstream before didChange, downstream computes on stale state.
```

**Solution:**
Per-downstream single send queue ensures:
```
didChange(v10) → completion  (in downstream read order)
```

**Implementation:**
- Each `BridgeConnection` serializes writes via `Mutex<ChildStdin>`
- Notifications and requests share the same write path, preserving order
- For document-level parallelism (future optimization): separate queues per `(downstream, document_uri)`

### 7. Request Handling During Initialization

#### 7.1 The Initialization Window Race Condition

**Problem: Client sends requests before bridge downstream is ready**

kakehashi responds to client's `initialize` immediately, independent of downstream bridge connections. This creates a race condition.

**Bridge spawn timing:** When kakehashi detects embedded language blocks (e.g., Python in markdown), it spawns the corresponding downstream LS (e.g., pyright). Detection can occur on `didOpen` or `didChange`.

```
┌────────┐     ┌──────────────┐     ┌──────────┐
│ Client │     │ kakehashi│     │ pyright  │
└───┬────┘     └──────┬───────┘     └────┬─────┘
    │                 │                  │
    │──initialize────▶│                  │
    │◀──result────────│                  │  (immediate response)
    │──initialized───▶│                  │
    │                 │                  │
    │──didOpen(md)───▶│                  │
    │                 │ (parse, find Python block)
    │                 │──initialize─────▶│  (spawn pyright)
    │──hover(Python)─▶│                  │
    │                 │──hover──────────▶│  ← RACE: pyright not ready!
    │                 │                  │
    │                 │◀──init result────│
    │                 │──initialized────▶│
    │                 │──didOpen────────▶│  (kakehashi sends, not client)
    │                 │                  │
    ════════════════════════════════════════════ didOpen sent
    │                 │                  │
    │──hover(Python)─▶│──hover──────────▶│  ← Normal: just forward
    │◀──result────────│◀──result─────────│
```

**Key insight:** The client never sends `didOpen` to the downstream LS. kakehashi is responsible for sending `didOpen` to the bridge downstream. This defines two distinct phases:

| Phase | Duration | Request Handling |
|-------|----------|------------------|
| **Initialization Window** | spawn → didOpen sent | Special handling required (see below) |
| **Normal Operation** | after didOpen sent | Simple pass-through, no queuing needed |

#### 7.2 Request Handling Strategies

**During Initialization Window, request handling strategy depends on request semantics:**

| Category | Requests | Behavior during Init Window | Rationale |
|----------|----------|-------------------------------|-----------|
| **Incremental** | completion, signatureHelp, hover | Request superseding + bounded wait (default 5s) + timeout fail | Stale results are useless, but user may be waiting; bound wait prevents hangs |
| **Explicit action** | definition, references, rename, codeAction, formatting | Wait with timeout (5s) | User explicitly requested, waiting is expected |

**Note:** After `didOpen` is sent (Normal Operation), all requests are simply forwarded. No special handling needed.

#### 7.3 Request Superseding Pattern (for Incremental Requests)

Incremental requests should NOT immediately return empty. The user might be waiting for the result. Instead, use a **request superseding** pattern:

1. If not initialized, keep the request pending but attach a **bounded wait** (default 5s; configurable per bridge) to avoid indefinite hangs.
2. If a **new request of the same type** arrives before the previous one is processed, return `REQUEST_FAILED` for the older request with a `"superseded"` reason (no silent drop).
3. When initialization completes (or the wait expires), process the most recent pending request; if initialization is still incomplete at the deadline, fail it with `REQUEST_FAILED` and a clear timeout message.

**LSP compliance rationale:**
- **Every request gets a response** (LSP 3.x § Request Message) - no dropped requests
- Uses `REQUEST_FAILED` (-32803) for superseded requests (LSP 3.17+)
- The request failed due to changed client state (newer request arrived), making the result obsolete

**LSP note:** The spec encourages clients to cancel when their own state changes; this bridge-side superseding keeps UX responsive during the initialization window while still returning a standards-compliant error. Client-driven `$/cancelRequest` remains supported and should cancel any downstream request already in-flight.

```
Scenario A: User is waiting
  "pri" → completion① (pending)
  ... user waits ...
  (initialization complete)
  → Process completion①, return result ✓  User gets their result

Scenario B: User continues typing (request superseding)
  "pri" → completion① (pending)
  "print" → completion② arrives
  → Send REQUEST_FAILED error for completion①
  → Keep completion② pending
  (initialization complete)
  → Process completion②, return result ✓  Only latest result matters
```

**Error response for superseded requests:**
```rust
ResponseError {
    code: ErrorCodes::REQUEST_FAILED,  // -32803 (LSP 3.17)
    message: "Request superseded by newer request of same type".to_string(),
    data: Some(json!({"reason": "incremental_request_superseded"})),
}
```

**Implementation:**

```rust
/// Wait for initialization with timeout (explicit actions and incremental)
async fn wait_for_initialized(&self, timeout: Duration) -> Result<(), ResponseError> {
    if self.initialized.load(Ordering::SeqCst) {
        return Ok(());
    }

    tokio::select! {
        _ = self.initialized_notify.notified() => Ok(()),
        _ = tokio::time::sleep(timeout) => {
            Err(ResponseError {
                code: ErrorCodes::REQUEST_FAILED,
                message: "Timeout waiting for downstream language server initialization".to_string(),
                data: None,
            })
        }
    }
}

/// Track pending incremental requests (only latest is kept)
struct PendingIncrementalRequests {
    completion: Option<(i64, oneshot::Sender<Result<Value, ResponseError>>)>,
    signature_help: Option<(i64, oneshot::Sender<Result<Value, ResponseError>>)>,
    hover: Option<(i64, oneshot::Sender<Result<Value, ResponseError>>)>,
}

impl PendingIncrementalRequests {
    /// Register new request, sending REQUEST_FAILED to any existing one of the same type
    fn register_completion(&mut self, id: i64, sender: oneshot::Sender<Result<Value, ResponseError>>) {
        if let Some((old_id, old_sender)) = self.completion.take() {
            // Send REQUEST_FAILED error to superseded request (LSP 3.17 compliant)
            let _ = old_sender.send(Err(ResponseError {
                code: ErrorCodes::REQUEST_FAILED,
                message: "Request superseded by newer request of same type".to_string(),
                data: Some(json!({"reason": "incremental_request_superseded"})),
            }));
            log::debug!("Sent REQUEST_FAILED for stale completion request {}", old_id);
        }
        self.completion = Some((id, sender));
    }
}

pub async fn send_request(&self, method: &str, params: Value) -> Result<Value, ResponseError> {
    match method {
        // initialize: no wait needed
        "initialize" => {}
        // All other requests: wait with timeout
        _ => {
            self.wait_for_initialized(Duration::from_secs(5)).await?;
        }
    }

    // Request superseding for incremental requests is handled separately:
    // PendingIncrementalRequests.register_*() sends REQUEST_FAILED to older requests
    // before this new request is processed

    // ... proceed with request
}
```

**Timeout considerations:**

| Category | Timeout needed? | Rationale |
|----------|-----------------|-----------|
| **Incremental** (request superseding) | Yes (default 5s, configurable) | Prevents indefinite waits during initialization; latest request still processed or explicitly failed |
| **Explicit action** | Yes (5s) | User is explicitly waiting; need feedback if server is broken |

### 8. Response Aggregation Strategies

For fan-out **requests** (with `id`), configure aggregation per method:

```rust
enum AggregationStrategy {
    /// Return first successful response, cancel others
    FirstWins,

    /// Wait for all, merge array results (e.g., completion items, code actions).
    /// **Note**: Merging can be complex. Simple concatenation may lead to
    /// duplicates (completions) or conflicting text edits (code actions).
    /// The initial implementation should favor sophisticated deduplication where
    /// possible and document the risks of conflicting edits.
    MergeAll {
        dedup_key: Option<String>,  // e.g., 'label' for completions
        max_items: Option<usize>,   // limit total items
    },

    /// Wait for all, return highest priority non-null result
    Ranked {
        priority: Vec<String>,  // server keys in priority order
    },
}
```

**When aggregation IS needed:**
- `completion`: Both LSes return completion items → merge into single list
- `codeAction`: pyright refactorings + ruff lint fixes → merge into single list

**Aggregation stability rules:**
- **Per-LS deadlines**: Each downstream request has a configurable timeout (default: 5s explicit, 2s incremental). On timeout, aggregator returns whatever results are available. If at least one downstream succeeds, respond with a successful `result` that contains merged items plus partial metadata in-band (e.g., `{ "items": [...], "partial": true, "missing": ["ruff"] }`). If all downstreams fail or time out, respond with a single `ResponseError` (`REQUEST_FAILED`) describing the missing/unhealthy servers. Never send both `result` and `error` in the same response.
- **Cancel in spite of non-compliant servers**: Send `$/cancelRequest` to slow downstream servers, but do not wait indefinitely because servers may ignore `$` notifications per LSP. The aggregator’s timeout is the hard ceiling for the upstream response.
- **Partial results are explicit**: Partial metadata must live inside the successful `result` payload (not an `error` field) to keep the wire response LSP-compliant while still surfacing degradation.

**When aggregation is NOT needed:**
- Single capable LS → route directly
- Diagnostics → notification pass-through (client aggregates per LSP 3.x § Publish Diagnostics)
- Capabilities don't overlap → route to respective LS

### 9. Notification Pass-through (No Aggregation)

**Diagnostics and other server-initiated notifications do NOT require aggregation.**

`textDocument/publishDiagnostics` is a notification (no `id`, no response expected). Each downstream server can publish its own diagnostics independently:

```
pyright  ──publishDiagnostics──►  bridge  ──publishDiagnostics──►  upstream
ruff     ──publishDiagnostics──►  bridge  ──publishDiagnostics──►  upstream
                                  (pass-through, no merge)
```

The bridge simply:
1. Receives notification from downstream
2. Transforms URI (virtual → host document URI)
3. Forwards to upstream client

The client (e.g., VSCode) automatically aggregates diagnostics from multiple sources. This is the standard LSP behavior and requires no special handling.

**Other pass-through notifications:**
- `$/progress` — Already forwarded via `notification_sender` channel
- `window/logMessage` — Can be forwarded as-is
- `window/showMessage` — Can be forwarded as-is

### 10. Cancellation Propagation

When upstream sends `$/cancelRequest` (LSP 3.x § Cancellation Support), propagate to all downstream servers that have pending requests for that `upstream_id`:

```rust
// In LanguageServerPool
async fn handle_cancel_request(&self, upstream_id: i64) {
    // Find all pending downstream requests for this upstream_id
    for (downstream_key, downstream_id) in self.pending_correlations.get(&upstream_id) {
        if let Some(conn) = self.connections.get(&downstream_key) {
            // Send $/cancelRequest to downstream
            let _ = conn.send_notification("$/cancelRequest", json!({
                "id": downstream_id
            })).await;
        }
    }
    // Clean up pending entries
    self.pending_correlations.remove(&upstream_id);
}
```

**Downstream non-compliance:** Servers may legally ignore `$/cancelRequest` (LSP § `$` notifications). Timeouts on fan-out aggregation remain the hard ceiling to guarantee the upstream request still completes.

**Tracking Structure:**

```rust
/// Maps upstream request ID to downstream request IDs for cancellation
pending_correlations: DashMap<i64, Vec<(String, i64)>>, // upstream_id → [(downstream_key, downstream_id)]
```

**Upstream response to cancellation:** Always return a response to the client after propagating cancellation. Use the standard LSP `RequestCancelled` code (-32800) when the method is server-cancellable; otherwise use `REQUEST_FAILED` with a `"cancelled"` message. Never leave the upstream request pending—cancellation must still round-trip a response per LSP.

### 11. Circuit Breaker Pattern

Prevent cascading failures when a downstream server is unhealthy:

```rust
struct CircuitBreaker {
    state: AtomicU8,           // 0=Closed, 1=Open, 2=HalfOpen
    failure_count: AtomicU32,
    last_failure: AtomicU64,   // timestamp
    config: CircuitBreakerConfig,
}

struct CircuitBreakerConfig {
    failure_threshold: u32,     // failures before opening (default: 5)
    reset_timeout_ms: u64,      // time before half-open (default: 30000)
    success_threshold: u32,     // successes in half-open to close (default: 2)
}
```

**State Transitions:**

```
     ┌─────────────────────────────────────┐
     │                                     │
     ▼                                     │
  Closed ──[failure_threshold]──► Open ────┘
     ▲                              │
     │                              │ [reset_timeout]
     │                              ▼
     └────[success_threshold]── HalfOpen
```

**Integration:**

```rust
impl LanguageServerPool {
    async fn send_request_with_circuit_breaker(
        &self,
        key: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ResponseError> {
        let breaker = self.circuit_breakers.entry(key.to_string())
            .or_insert_with(|| CircuitBreaker::new(self.breaker_config.clone()));

        if !breaker.allow_request() {
            log::warn!("Circuit breaker open for {}, skipping request", key);
            // LSP compliance: Every request must receive a response
            return Err(ResponseError {
                code: ErrorCodes::REQUEST_FAILED,
                message: format!(
                    "Downstream server '{}' is unhealthy (circuit breaker open)",
                    key
                ),
                data: None,
            });
        }

        match self.send_request_inner(key, method, params).await {
            Ok(response) => {
                breaker.record_success();
                Ok(response)
            }
            Err(e) => {
                breaker.record_failure();
                log::warn!("Request to {} failed: {}", key, e);
                // LSP compliance: Return error response, not None
                Err(ResponseError {
                    code: ErrorCodes::REQUEST_FAILED,
                    message: format!("Request to downstream server '{}' failed: {}", key, e),
                    data: None,
                })
            }
        }
    }
}
```

### 12. Bulkhead Pattern

Isolate downstream servers so one slow/broken server doesn't exhaust shared resources:

```rust
struct BulkheadConfig {
    max_concurrent: usize,      // max concurrent requests per downstream (default: 10)
    queue_size: usize,          // max queued requests before rejection (default: 50)
}

// Per-connection semaphore
struct BridgeConnection {
    // ... existing fields ...
    request_semaphore: Semaphore,  // limits concurrent requests
}
```

**Integration with existing `MAX_PENDING_REQUESTS`:**

The current `MAX_PENDING_REQUESTS = 100` acts as a global backpressure limit. Bulkhead adds per-connection limits for finer isolation.

**Overflow handling:** If acquiring the semaphore or enqueueing would exceed `max_concurrent` + `queue_size`, fail immediately with `REQUEST_FAILED` (or `SERVER_CANCELLED` when the method is declared server-cancellable) and a clear message like “bulkhead limit reached for pyright”. Do not enqueue the request, and clean up any correlation tracking to avoid dangling cancellations.

### 13. Configuration Example

```yaml
# Configuration example (routing-first approach)
#
# Server discovery: languageServers with matching `languages` field are
# automatically used for that injection language. No explicit server list
# needed in bridges config.
#
# Priority order can be explicitly configured. If `priority` is omitted,
# it defaults to alphabetical order of server names.

languages:
  markdown:
    bridges:
      python:
        # Servers discovered from languageServers with languages: [python]
        priority: ["ruff", "pyright"] # Explicitly prioritize ruff
        # Default: single_by_capability routing (no aggregation config needed)
        #
        # Only configure methods that need non-default behavior:
        aggregations:
          textDocument/completion:
            strategy: concatenated   # both LSes return items → merge
            dedup_key: label
          textDocument/codeAction:
            strategy: concatenated   # both LSes return actions → merge
          # hover, definition: use default (single_by_capability, no config)

languageServers:
  pyright:
    cmd: [pyright-langserver, --stdio]
    languages: [python]              # ← auto-discovered for python bridges
  ruff:
    cmd: [ruff, server]
    languages: [python]              # ← auto-discovered for python bridges
```

## Consequences

### Positive

- **LSP Compliance**: All error handling uses standard LSP error codes (REQUEST_FAILED, SERVER_NOT_INITIALIZED)
- **No Dropped Requests**: Every request receives a response, even during failures (circuit breaker open, timeout, etc.)
- **Request Superseding**: Stale incremental requests receive LSP-compliant REQUEST_FAILED errors when superseded by newer requests
- **Graceful Degradation**: Partial initialization failures allow working servers to continue serving requests
- **Bounded Waits**: Timeouts on initialization and aggregation prevent hangs while still returning best-effort results
- **Routing-First Simplicity**: Most requests go to a single LS — no aggregation overhead for common cases
- **Minimal Configuration**: Default capability-based routing works without per-method config
- **Multi-LS Support**: Python files can use pyright + ruff simultaneously
- **Fault Isolation**: One crashed LS doesn't affect others (circuit breaker + bulkhead)
- **Cancellation Propagation**: Client cancellations propagated to all downstream servers
- **Flexible Aggregation**: Per-method control over how responses are combined (when needed)
- **Backward Compatible**: Single-LS configurations continue to work unchanged

### Negative

- **Complexity**: More state to manage (correlations, circuit breakers, aggregators)
- **Configuration Surface**: Users need to understand aggregation strategies for overlapping capabilities
- **Aggregation Complexity**: Merging candidate lists (completion, codeAction) requires deduplication logic to handle cases where multiple servers propose similar items. The challenge is in deduplication heuristics - different servers may propose similar-looking candidates with subtle differences (different labels, kinds, or edit details), making it hard to decide what counts as a "duplicate". Note that merging candidate lists is safe since users select one item; conflicts only arise if the bridge were to automatically apply multiple edits simultaneously (which is not the design).
- **Latency**: Fan-out with `concatenated` waits up to per-server timeouts; partial results may surface instead of complete lists
- **Memory**: Tracking pending correlations adds overhead

### Neutral

- **Existing Tests**: Current single-LS tests remain valid
- **Incremental Adoption**: Routing-first means aggregation can be added later for specific methods
- **Diagnostics**: Pass-through by design — client handles aggregation

## Implementation Plan

The current async bridge has **hang issues** due to waker/channel race conditions. Multiple fix attempts have not resolved the root cause.
**Decision: Re-implement from scratch with simpler, proven patterns.**

### Phase 1: Single-LS-per-Language Foundation

**Scope**: Support **one language server per language** (multiple languages supported, but each language uses only one LS)

```
kakehashi (host)
  ├─→ pyright  (Python only)
  ├─→ lua-ls   (Lua only)
  └─→ sqlls    (SQL only)
```

**What works in Phase 1:**
- **LSP Compliance** (§1): Every request receives a response using standard error codes (`REQUEST_FAILED`, `SERVER_NOT_INITIALIZED`)
- **Multiple embedded languages** in same document (Python, Lua, SQL blocks in markdown)
- **Parallel initialization** of multiple LSes (§5.2): Each LS initializes independently with no global barrier
- **Two-Phase notification handling** (§6.1):
  - Phase 1 guard: Block all notifications before `initialized` (except `initialized` itself) with `SERVER_NOT_INITIALIZED`
  - Phase 2 guard: Drop document notifications (`didChange`, `didSave`) before `didOpen` sent; state accumulated in `didOpen`
  - Per-downstream snapshotting: Late initializers receive latest document state, not stale snapshot
- **Initialization race handling** (§7):
  - Request superseding pattern for incremental requests (completion, hover, signatureHelp)
  - Bounded wait with timeout (default 5s) for all requests during initialization window
  - Superseded requests receive LSP-compliant `REQUEST_FAILED` error
- **Notification ordering guarantees** (§6.2): Serialized writes per connection with queue prioritization (text sync before long requests)
- **Routing errors surfaced** (§2): `REQUEST_FAILED` when no provider exists (no silent `null`)
- **Simple routing**: language → single LS (no aggregation needed)

**What Phase 1 does NOT support:**
- Multiple LSes for same language (e.g., Python → pyright + ruff)
- Fan-out / scatter-gather for requests
- Response aggregation/merging

**Exit Criteria:**
- All existing single-LS tests pass without hangs
- Can handle Python, Lua, SQL blocks simultaneously in markdown
- No initialization race failures under normal conditions

### Phase 2: Resilience Patterns (Stability Before Complexity)

**Scope**: Add fault isolation and recovery patterns to **single-LS-per-language** setup before adding multi-LS complexity

**Why Phase 2 before Multi-LS:**
- Resilience patterns work with simple single-LS architecture
- Stabilize foundation before adding aggregation complexity
- Circuit breaker and bulkhead become MORE critical with multi-LS (Phase 3)
- Better to debug resilience issues without aggregation layer

**What Phase 2 adds:**
- **Circuit Breaker** (§11): Prevent cascading failures when downstream LS is unhealthy
  - Open circuit after failure threshold (default: 5 failures)
  - Half-open state for recovery testing
  - Fast-fail with `REQUEST_FAILED` instead of waiting for timeouts
  - LSP compliance: Every blocked request receives error response
- **Bulkhead Pattern** (§12): Isolate downstream servers to prevent resource exhaustion
  - Per-connection semaphore (max concurrent requests, default: 10)
  - Queue size limits before rejection (default: 50)
  - Overflow handling: Immediate failure with `REQUEST_FAILED` (or `SERVER_CANCELLED` for server-cancellable methods)
  - Prevent one slow LS from blocking others
- **Per-server timeout configuration**: Custom timeout per LS type, applied as hard ceilings for requests (including aggregation later)
- **Health monitoring**: Track LS health metrics, log warnings for flaky servers
- **Partial-result metadata**: When a timeout occurs, return available results and flag partial response in `data` (keeps UX responsive while making degradation visible)

**Exit Criteria:**
- Circuit breaker opens/closes correctly when LS crashes/recovers
- Bulkhead prevents slow LS from blocking other languages
- Timeouts fire and surface partial-result metadata without leaving requests hanging
- System remains responsive even when one LS is unhealthy

### Phase 3: Multi-LS-per-Language with Aggregation

**Scope**: Extend to support **multiple language servers per language** with routing and aggregation

```
kakehashi (host)
  └─→ Python blocks
        ├─→ pyright  (type checking, completion) ← Circuit breaker from Phase 2
        └─→ ruff     (linting, formatting)       ← Bulkhead from Phase 2
             ↓
        [RequestRouter + ResponseAggregator] ← New in Phase 3
```

**What Phase 3 adds:**
- **Routing strategies** (§4): single-by-capability (default) and fan-out
- **Response aggregation** (§8): preferred, concatenated, deduplicated, … strategies
  - **Aggregation complexity**: Deduplication heuristics for candidate lists (completion, codeAction)
  - Challenge: Different servers may propose similar items with subtle differences (labels, kinds, edit details)
  - Safe by design: Users select one item from merged list; no conflicting edits applied simultaneously
- **Per-method aggregation configuration** (§13): Configure only methods that need non-default behavior
- **Cancellation propagation** (§10): Propagate `$/cancelRequest` to all downstream LSes with pending requests
- **Fan-out skip/partial** (§5.3): Unhealthy or uninitialized servers skipped in aggregation; responses include partial metadata identifying missing servers
- **Leverages Phase 2 resilience**: Each LS in multi-LS setup already has circuit breaker + bulkhead

**Exit Criteria:**
- Can use pyright + ruff simultaneously for Python
- Completion results merged from both LSes with deduplication working correctly
- CodeAction lists merged without duplicates or conflicting items in UI
- Routing config works (single-by-capability default, fan-out for configured methods)
- Resilience patterns work per-LS (pyright circuit breaker independent of ruff)
- Partial results surfaced when one LS times out or is unhealthy

**Rationale for phased approach:**
- Phase 1 delivers immediate value (multi-language support) with minimal complexity
- Phase 2 adds stability/resilience to simple architecture (easier to debug)
- Phase 3 adds multi-LS complexity on top of stable, resilient foundation
- Routing-first principle means most requests still use Phase 1 fast path (single LS)

## Related ADRs

- **[ADR-0006](0006-language-server-bridge.md)**: Core LSP bridge architecture
  - Establishes the fundamental 1:1 bridge pattern (host document → single language server per language)
  - Defines eager spawn strategy and position translation
  - ADR-0012 extends this to 1:N (host document → multiple language servers per language)

- **[ADR-0008](0008-language-server-bridge-request-strategies.md)**: Per-method bridge strategies
  - Defines four strategies: Parallel Fetch, Full Delegation, Edit Filtering, Background Collection
  - Specifies how different LSP methods should be handled (completion, hover, diagnostics, etc.)
  - **Relationship to ADR-0012**: ADR-0008's per-method strategies remain valid for single-LS routing. However, ADR-0012 clarifies multi-LS aspects:
    - **Diagnostics**: ADR-0008 suggested "merge & dedupe"; ADR-0012 specifies "pass-through" (client aggregates)
    - **Initialization window**: ADR-0008 assumes servers are initialized; ADR-0012 adds "request superseding" and "wait-with-timeout" patterns for requests during initialization
    - **Multi-server merging**: ADR-0012 provides concrete routing strategies (SingleByCapability, FanOut) and aggregation options (preferred, concatenated, deduplicated, …)

- **[ADR-0009](0009-async-bridge-architecture.md)**: Single-LS async architecture **(Superseded)**
  - Established tokio-based async I/O with request ID routing
  - Identified the need for async concurrency (tower-lsp dispatches concurrently)
  - **Why superseded**: Fundamental waker/channel race conditions proved unresolvable; ADR-0012 re-implements with simpler patterns
