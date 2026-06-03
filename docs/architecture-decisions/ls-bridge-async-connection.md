# ADR-0014: LS Bridge Async Connection

| | |
|---|---|
| **Status** | Draft |
| **Date** | 2026-01-05 |
| **Decision-makers** | atusy |

## Scope

This ADR defines how to communicate with **a single downstream language server** via stdio. It covers:
- Process spawning and I/O primitives
- Async reader/writer task patterns
- Connection-level timeout mechanisms
- Pending request lifecycle

**Out of Scope**: Coordination of multiple language servers (single-LS vs multi-LS per language) is covered by ADR-0016.

## Context

The LSP bridge connects injection regions to external language servers via stdio (ADR-0006). A language server is spawned as a child process with stdin/stdout streams for LSP JSON-RPC communication. The bridge must handle I/O from multiple concurrent requests on a single connection without blocking.

### Key Requirements

1. **Cancellation**: How to cleanly interrupt I/O operations on shutdown/timeout?
2. **Reliability**: How to detect dead or hung servers?
3. **Maintainability**: Sync vs async boundaries, idiomatic patterns

Three critical requirements drive this decision:
- **Zero extra OS threads per connection**: Avoid blocking OS threads on I/O
- **Clean cancellation**: Shutdown and timeout must interrupt blocked I/O without hanging
- **Idiomatic async**: Pure async codebase integrates cleanly with tower-lsp's async handlers

## Decision

**Use `tokio::process` with pure async I/O for all language server communication.**

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    tokio runtime                        │
│                                                         │
│  Per-server async tasks (green threads):                │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │              AsyncBridgeConnection              │    │
│  │                                                 │    │
│  │  ┌──────────────┐    ┌────────────────────────┐ │    │
│  │  │ send_request │    │     reader task        │ │    │
│  │  │   (async)    │    │                        │ │    │
│  │  └──────┬───────┘    │  select! {             │ │    │
│  │         │            │    line = read =>      │ │    │
│  │         ▼            │    _ = shutdown =>     │ │    │
│  │  ┌──────────────┐    │    _ = timeout =>      │ │    │
│  │  │ AsyncWrite   │    │  }                     │ │    │
│  │  │ (stdin)      │    │                        │ │    │
│  │  └──────────────┘    └────────────────────────┘ │    │
│  │         │                       │               │    │
│  └─────────┼───────────────────────┼───────────────┘    │
│            │                       │                    │
│            ▼                       ▼                    │
│     ┌─────────────────────────────────────┐             │
│     │           tokio reactor             │             │
│     │      (epoll/kqueue — no threads)    │             │
│     └─────────────────────────────────────┘             │
└─────────────────────────────────────────────────────────┘
              │                       │
              ▼                       ▼
       ┌──────────────────────────────────┐
       │         rust-analyzer            │
       │          (subprocess)            │
       └──────────────────────────────────┘
```

### Key Components

**Process Management:**
- Spawn language servers using `tokio::process::Command`
- Use `tokio::io::AsyncBufReadExt` and `tokio::io::AsyncWriteExt` for async stdin/stdout operations

**Reader Task Pattern:**
- Run a dedicated async reader task per server using `select!` to multiplex:
  - Reading responses from server stdout
  - Shutdown signals
  - Timeout detection
- Route responses to pending request handlers via shared map

**Writer Pattern:**
- Write requests to server stdin using async mutex-protected writer
- Single writer task ensures no byte-level corruption

**Pending Request Cleanup:**
When the reader task exits abnormally (EOF, read error, timeout, or shutdown), all pending requests must receive error responses:
- Fail all pending requests with `INTERNAL_ERROR` (-32603)
- Clear the pending map to prevent memory leaks
- Log failures for observability

**Cleanup Timeout Bounds:**
The cleanup operation itself must complete within a bounded time (recommended: 100ms) to prevent cleanup hangs from blocking state transitions:
- If cleanup exceeds timeout, force state transition anyway
- Log cleanup timeout as a warning (indicates potential channel saturation)
- Remaining pending entries will be dropped when the pending map is dropped

**Note on Dropped Channels:** When a `oneshot::Sender` is dropped without sending (due to cleanup timeout), the receiver's `.await` returns `RecvError`. This is semantically equivalent to receiving `INTERNAL_ERROR` - the client knows the request failed. Callers should handle both explicit error responses and channel errors uniformly.

**Race Prevention (Request Registration vs Reader Exit):**

A critical race condition exists between request registration and reader task cleanup. The **check-insert-check pattern** prevents orphaned requests:

```rust
async fn send_request(&self, request: Request) -> Result<Response> {
    // FIRST CHECK: Fail fast if connection not ready
    if self.connection_state.load() != ConnectionState::Ready {
        return Err(Error::ConnectionNotReady);
    }

    let request_id = request.id;  // LSP spec: integer | string
    let (response_tx, response_rx) = oneshot::channel();
    self.pending.insert(request_id, response_tx);

    // SECOND CHECK: Detect race with cleanup
    if self.connection_state.load() != ConnectionState::Ready {
        self.pending.remove(&request_id);
        return Err(Error::ConnectionNotReady);
    }

    // Safe: cleanup either hasn't started or will include our entry
    self.write_request(request_id, request).await?;
    response_rx.await
}
```

**Error Code Mapping**: `Error::ConnectionNotReady` maps to `REQUEST_FAILED` (-32803) with state-specific messages. See ADR-0015 § Operation Gating for the complete mapping.

### Timeout Architecture

The system uses two distinct timeout mechanisms with different purposes:

**1. Liveness Timeout (Server Health Monitor)**

- **Purpose**: Detect zombie servers (process alive but unresponsive to pending requests)
- **Scope**: Connection-level health monitoring
- **State-Based Gating**:
  - **Disabled** during: Initializing, Closing, Failed, Closed states, or Ready with pending = 0
  - **Enabled** during: Ready state with pending requests > 0
- **Timer Lifecycle**:
  - **Start**: First request sent when pending count transitions 0→1
  - **Keep running**: Additional requests sent (pending count increases)
  - **Reset**: Any stdout activity (response or notification) while active
  - **Stop**: Last response received (pending count returns to 0)
- **Behavior on Timeout**: Connection transitions to Failed state

**2. Initialization Timeout (Startup Bound)**

- **Purpose**: Bound initialization time to prevent indefinite hangs during server startup
- **Scope**: Single operation during connection startup
- **Duration**: Longer than liveness timeout (typically 30-60 seconds)
- **Timer Management**:
  - **Start**: When initialize request sent (Connection state: Initializing)
  - **Stop**: When initialize response received (transition to Ready)
  - **Cancel**: On shutdown signal (global shutdown timeout takes over, see ADR-0018)
- **Behavior on Timeout**: Connection transitions to Failed state

**Independence**: The two timeouts serve different purposes and never overlap (liveness disabled during Initializing; initialization timeout disabled once Ready).

**Coordination with Other Timeouts**: See [ADR-0018](0018-ls-bridge-timeout-hierarchy.md) for precedence rules when shutdown timeout is active.

## Consequences

### Positive

**Zero Extra OS Threads Per Connection:**
- tokio reactor monitors file descriptors in a single event loop
- Each connection uses async tasks (green threads), not OS threads
- Multiple connections share the tokio runtime's thread pool

**Clean Cancellation:**
- `select!` macro unifies shutdown, timeout, and read in one construct
- No blocked system calls that ignore cancellation signals
- Immediate shutdown even when server is silent

**Dead Server Detection:**
- Liveness timeout detects hung servers without separate monitoring
- EOF on stdout automatically detected and propagated

**Idiomatic Async Patterns:**
- Pure async codebase with no sync/async boundary crossing
- Compatible with tower-lsp's async request handlers

**Concurrent Requests:**
- Multiple in-flight requests on same connection supported
- Pending map tracks request-response correlation

### Negative

**API Differences:**
- `tokio::process::Command` has different API than `std::process::Command`
- Requires understanding of tokio's async I/O primitives

**Runtime Dependency:**
- Requires tokio runtime with multi-threaded executor
- Already a dependency via tower-lsp, so minimal impact

### Neutral

**Async Task Overhead:**
- Two async tasks per language server (reader + shared writer access)
- Green threads are lightweight (~2KB stack), not OS threads

## Alternatives Considered

### Alternative 1: std::process with Background OS Threads

Use standard library's `std::process` with one blocking OS thread per server reading stdout.

**Rejected Reasons:**

1. **Shutdown Bug**: Blocked `read_line()` call ignores shutdown flag
   ```rust
   loop {
       if shutdown.load(SeqCst) { break; }  // Never reached if...
       reader.read_line(&mut buf);          // ...blocked here forever
   }
   ```

2. **Thread Overhead**: One OS thread per connection wasted blocked on I/O

3. **Mixed Sync/Async**: Requires `blocking_send`, `spawn_blocking` for boundary crossing

4. **Manual Timeout Logic**: Complex and error-prone to implement correctly

**Comparison:**

| Aspect | Background Thread | tokio async |
|--------|-------------------|-------------|
| OS thread usage | 1 per connection | 0 per connection |
| Shutdown while idle | ❌ Hangs on read | ✅ Clean exit via `select!` |
| Timeout handling | Manual, error-prone | Built into `select!` |

## Related Decisions

- **ADR-0006**: Core LSP bridge architecture (pooling, spawn strategy)
- **[ADR-0015](0015-ls-bridge-message-ordering.md)**: Message Ordering (built on this I/O layer)
- **[ADR-0016](0016-ls-bridge-server-pool-coordination.md)**: Server Pool Coordination (uses this I/O foundation for N servers)
- **[ADR-0017](0017-ls-bridge-graceful-shutdown.md)**: Graceful Shutdown (uses shutdown signal from `select!`, adds LSP handshake and process cleanup)
- **[ADR-0018](0018-ls-bridge-timeout-hierarchy.md)**: Timeout Hierarchy (coordinates liveness timeout with other timeout systems)

## Notes

**Clarification on "Zero Extra Threads":**
- Refers to zero extra **OS threads** per connection, not zero async tasks
- Each connection uses two async tasks (reader + writer)
- Async tasks are green threads (~2KB stack), multiplexed by tokio runtime

**Verification:**
- Unit test: `select!` correctly handles concurrent read + shutdown + timeout
- Integration test: Shutdown while server silent → connection closes cleanly

## Amendment History

- **2026-01-06**: Merged [Amendment 001](0013-async-io-layer-amendment-001.md) - Added pending request cleanup requirements and race prevention pattern to prevent indefinite client hangs on reader task exit
- **2026-01-06**: Merged [Amendment 002](0013-async-io-layer-amendment-002.md) - Added state-based liveness timeout gating and separate initialization timeout mechanism to prevent liveness timeout from firing during slow initialization
