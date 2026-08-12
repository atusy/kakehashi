# LS Bridge Async Connection

## Scope

This decision defines how to communicate with **a single downstream language server** via stdio. It covers:
- Process spawning and I/O primitives
- Async reader/writer task patterns
- Connection-level timeout mechanisms
- Pending request lifecycle

**Out of Scope**: Coordination of multiple language servers (single-LS vs multi-LS per language) is covered by ls-bridge-server-pool-coordination.

## Context

The LSP bridge connects injection regions to external language servers via stdio (language-server-bridge). A language server is spawned as a child process with stdin/stdout streams for LSP JSON-RPC communication. The bridge must handle I/O from multiple concurrent requests on a single connection without blocking.

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
│  │         │            │    result = read_msg =>│ │    │
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

**Cancel-safety of the read branch:** multiplexing means any other branch
completing drops the read mid-frame, so the framing parser must be
cancel-safe (§ Invariants). `BridgeReader::frame` is where that partial-frame
progress lives.

**Framing size ceilings** (amended with bridge-routing-protocol; **target
state** — today's `BridgeReader` enforces none of these bounds and
allocates the declared `Content-Length` unchecked, which is exactly the
exposure this amendment closes; the ceilings land with that protocol's
implementation): the reader
enforces three incrementally checked bounds — a maximum header-line length, a
maximum total header-block size, and a maximum declared `Content-Length` —
each violation being a framing error with the same fatal disposition as every
other framing violation: the connection fails; an oversized body is never
drained (draining an attacker-sized body can hang the reader), and the
header-line bound is enforced as bytes accumulate, never after an unbounded
buffer already grew. The body ceiling's default is implementation-defined and
deliberately generous — well above the largest legitimate payloads observed
(multi-megabyte diagnostics bursts are real) — so it trips on runaway or
adversarial peers, not on big workspaces; a configuration knob can follow if a
legitimate deployment ever meets it. The header-line and header-block
ceilings are likewise implementation-defined, in the small-kilobytes class —
LSP headers are few and tiny, so any legitimate margin is enormous. A peer
whose honest traffic exceeds a ceiling fails repeatedly through
acquire-driven respawn — or, for a connection nothing re-acquires (a
`forceStart`-only policy server with `languages = []`), stays unavailable
until a reload or an explicit restart — accepted: such a peer is
indistinguishable from a runaway one at the framing layer.

**Writer Pattern:**
- Single writer task ensures no byte-level corruption

**Pending Request Cleanup:**
When the reader task exits abnormally (EOF, read error, timeout, or shutdown), every pending request must receive a terminal error response:
- Normally `INTERNAL_ERROR` (-32603) — but a request already cancelled while
  still queued keeps `REQUEST_CANCELLED` (-32800), so the caller learns why
  it actually ended
- Clear the pending map to prevent memory leaks
- Log failures for observability

**Cleanup Timeout Bounds** (**target state** — today's cleanup drains every
pending entry synchronously, with no deadline and no overrun warning):
Cleanup is bounded (duration implementation-defined, in the sub-second
class), because it must never block a state transition:
- If cleanup exceeds its bound, the state transition happens anyway
- Log the overrun as a warning (it indicates potential channel saturation)
- Any pending entry cleanup did not reach is failed by the loss of its reply
  path, which callers treat identically to an explicit error — a caller
  cannot, and need not, distinguish the two

**Race Prevention (Request Registration vs Reader Exit):**

Registration and reader-task cleanup race, and the request that registers in
the window between cleanup's sweep and its state transition is the one that
waits forever. Registration therefore commits atomically with respect to
cleanup: a request either registers before the sweep — and is failed by it —
or is refused with `Error::ConnectionNotReady`. It may never land after the
sweep and survive.

**Error Code Mapping**: `Error::ConnectionNotReady` maps to `REQUEST_FAILED` (-32803) with state-specific messages. See ls-bridge-message-ordering § Operation Gating for the complete mapping.

### Timeout Architecture

The system uses two distinct timeout mechanisms with different purposes:

**1. Liveness Timeout (Server Health Monitor)**

- **Purpose**: Detect zombie servers (process alive but unresponsive to pending requests)
- **Scope**: Connection-level health monitoring
- **State-Based Gating**:
  - **Disabled** during: Initializing, Closing, Failed, Closed states, or Ready with no liveness-classified request outstanding
  - **Enabled** during: Ready state with at least one liveness-classified managed request outstanding
  - **Pass-through requests are excluded from liveness accounting
    entirely** — they carry no bridge-imposed timeout, so a slow one must
    never drive a healthy connection to `Failed`. **Target state**: the
    per-entry classification is an implementation precondition recorded in
    bridge-client-control-protocol — today's router counts every
    non-cancelled pending entry, and this rule lands with that protocol
- **Timer Lifecycle**:
  - **Start**: When the first liveness-classified request becomes outstanding
  - **Keep running**: While any remains outstanding
  - **Reset**: Each framed and JSON-decoded downstream message while active
    (the reset happens before the message is classified, so server-initiated
    requests count too). Not raw stdout activity: a downstream that dribbles
    out a partial frame resets nothing and is still caught, which is what makes
    the timer the backstop for a server that stalls mid-frame.
  - **Stop**: When the last one is answered
- **Behavior on Timeout**: Connection transitions to Failed state

**2. Initialization Timeout (Startup Bound)**

- **Purpose**: Bound initialization time to prevent indefinite hangs during server startup
- **Scope**: Single operation during connection startup
- **Duration**: Longer than liveness timeout (typically 30-60 seconds)
- **Timer Management**:
  - **Start**: When initialize request sent (Connection state: Initializing)
  - **Stop**: When initialize response received (transition to Ready)
  - **Cancel**: On shutdown signal (global shutdown timeout takes over, see ls-bridge-timeout-hierarchy)
- **Behavior on Timeout**: Connection transitions to Failed state

**Independence**: The two timeouts serve different purposes and never overlap (liveness disabled during Initializing; initialization timeout disabled once Ready).

**Coordination with Other Timeouts**: See ls-bridge-timeout-hierarchy for precedence rules when shutdown timeout is active.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- **A read dropped mid-frame must lose no frame progress.** Multiplexing
  means any other branch completing drops the read, and a parser that keeps
  its progress in that dropped future loses a consumed header every time —
  then resyncs onto the message body and reports a framing error against a
  well-framed stream, killing a healthy connection and blaming the server.
- **An oversized body is never drained.** Draining an attacker-sized body
  hangs the reader, which is the same denial the size ceiling exists to
  prevent. (This and the bullet below are **target state** — today's
  `BridgeReader` enforces no ceilings at all; see § Framing size ceilings.)
- **Size ceilings are enforced as bytes accumulate.** A ceiling checked only
  after the fact has already let a hostile or broken peer make the
  allocation it exists to prevent.
- **Registration of a pending request is atomic with respect to cleanup.** A
  request either registers before cleanup's sweep — and is failed by it — or
  is refused. One that lands after the sweep waits forever, and nothing later
  will notice it.
- **Cleanup can never block a state transition.** A connection that cannot
  finish failing its pending requests must still be able to reach its
  terminal state.
- **The loss of a reply path means failure, not silence.** A caller must
  never need to distinguish an explicit error response from a dropped reply;
  both say the request failed.
- **Liveness must not be armed by requests it does not govern.** A
  pass-through carries no bridge-imposed timeout by contract, so counting one
  toward liveness lets a slow — but legitimate — request kill a healthy
  connection.

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

- **language-server-bridge**: Core LSP bridge architecture (pooling, spawn strategy)
- **[ls-bridge-message-ordering](ls-bridge-message-ordering.md)**: Message Ordering (built on this I/O layer)
- **[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)**: Server Pool Coordination (uses this I/O foundation for N servers)
- **[ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md)**: Graceful Shutdown (uses shutdown signal from `select!`, adds LSP handshake and process cleanup)
- **[ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md)**: Timeout Hierarchy (coordinates liveness timeout with other timeout systems)
- **[bridge-routing-protocol](bridge-routing-protocol.md)**: Motivated the framing size ceilings amendment (its answer-allocation bound depends on them)

## Notes

**Clarification on "Zero Extra Threads":**
- Refers to zero extra **OS threads** per connection, not zero async tasks
- Each connection uses two async tasks (reader + writer)
- Async tasks are green threads (~2KB stack), multiplexed by tokio runtime

**Verification:**
- Unit test: `select!` correctly handles concurrent read + shutdown + timeout
- Integration test: Shutdown while server silent → connection closes cleanly

## Amendment History

- **2026-01-06**: Merged Amendment 001 - Added pending request cleanup requirements and race prevention pattern to prevent indefinite client hangs on reader task exit
- **2026-01-06**: Merged Amendment 002 - Added state-based liveness timeout gating and separate initialization timeout mechanism to prevent liveness timeout from firing during slow initialization
- **2026-08-10**: Amendment — reader framing must be cancel-safe across `select!` wake-ups; partial-frame state moved into `BridgeReader`
- **2026-08-12**: Amendment (with bridge-routing-protocol) — framing size ceilings: incrementally enforced header-line, header-block, and `Content-Length` bounds, each a fatal framing error; see § Framing size ceilings
- **2026-08-12**: Applied the contract/invariant/mechanism discipline (template.md) - replaced the check-insert-check sketch and the pending-count arithmetic with the atomicity and exclusion rules they implement, and collected this ADR's traps (cancel-safe framing, never drain an oversized body, cleanup must not block a transition, liveness must not be armed by pass-through) into an Invariants section
