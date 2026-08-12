# LS Bridge Server Pool Coordination

**Related**:
- [ls-bridge-async-connection](ls-bridge-async-connection.md): Single-connection async I/O
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md): Single-server message ordering

**Phasing**: See ls-bridge-implementation-phasing — Phase 1 (routing), Phase 2 (rate-limited respawn), Phase 3 (aggregation).

## Scope

This decision defines how to coordinate **multiple downstream language server connections** from a single bridge. It covers:
- Server pool lifecycle (spawn, initialize, shutdown)
- Request routing to appropriate server(s)
- Document lifecycle per downstream server
- Notification handling (drop, forward, pass-through)

## Context

The bridge manages connections to multiple downstream language servers. Even in Phase 1, multiple servers exist (e.g., pyright for Python, lua-ls for Lua). Each connection follows ls-bridge-async-connection (async I/O) and ls-bridge-message-ordering (message ordering).

### Key Challenges

1. **Lifecycle Management**: How do we spawn, initialize, and shut down multiple servers?
2. **Request Routing**: Given a request for a language, which server should receive it?
3. **Document State**: How do we track document lifecycle per downstream server?
4. **Partial Failures**: What happens when some servers initialize successfully but others fail?

## Decision

**Adopt a phased approach: start with single-LS-per-language routing, extend to multi-LS aggregation in Phase 3.**

### Phase 1: Connection-Key Routing — `(server_name, root)` (Current)

Each language maps to a `server_name` via configuration. The pool is keyed by
`(server_name, resolved workspace root)` (a `ConnectionKey`), enabling:
- **Process sharing for related languages**: e.g., `typescript` and `typescriptreact` can share a single `tsgo` process (same `server_name`, same root)
- **Decoupling language from process**: The same binary can serve multiple languages
- **Per-root pooling for monorepos** (#382): the same `server_name` under two
  different marker roots gets two downstream processes, each rooted correctly;
  documents with no marker root share the client-root fallback connection.

Routing: `language` → `server_name` (via config) + document's resolved root → connection.
The root is resolved from the triggering document's `workspaceMarkers` walk
(`root_markers::resolve_marker_workspace`), the single source shared with the
spawn handshake so a connection's key always matches the root it was spawned at.
The `ConnectionKey` is stored on each connection handle, so the request,
`didChange`, host, and cancel paths route per-connection state via
`handle.key()` without re-resolving the root.

`completionItem/resolve`, `codeAction/resolve`, and `codeLens/resolve` carry
no `textDocument`, so the originating host URI is stashed in their routing
envelopes (`KakehashiEnvelope` / `CodeActionEnvelope` / `CodeLensEnvelope`)
and used to re-resolve the same `(server, root)` connection that produced the
item. (A legacy **completion** envelope without that field falls back to
the client-root connection — the pre-#382 rule, and still the shipped
behavior today, via the field's serde default; the code-action and
code-lens envelopes *require* the field, so a stamp-less one fails to
deserialize and the item is returned unresolved. The fail-soft rule
below is target state that lands with bridge-routing-protocol's
implementation.) Amended with
bridge-routing-protocol:
this re-resolution — like every site that derives a connection from the host
URI — consults the active route binding first, matched by the envelope's
stamp of the decided document's URI (virtual for virt items) and its
open incarnation; a missing, evicted, or mismatched **binding stamp**
fails soft under that target-state rule (distinct from the *legacy
envelope* above — an envelope missing the host-URI field entirely,
whose shipped client-root fallback predates bindings altogether) rather
than falling back to marker or
client-root resolution, which under a root override would reach the
config-root process instead of the one that produced the item.

**Shared-instance opt-in** (#391): a per-server `preferSharedInstance` boolean
(default `false`) routes a server's documents to one shared connection
(`ConnectionKey::shared`, kept distinct from the client-root fallback) instead
of one per marker root. Marker-less documents (non-file URIs such as an
editor's `untitled:` scratch documents, files with no marker up the tree, or
acquisitions with no document hint) join the shared instance too — the complete
client workspace (the folder snapshot, or the bare `rootUri` when the client
sent no folders) is announced on their behalf, so a shared connection spawned
under some marker root still learns the workspace such documents belong to. Keeping them on the client-root fallback would fork a
second process whose session-wide state (e.g. a completion corpus over every
open document) never meets the shared instance's. Root-JOINING is honored only when the downstream server advertises
`workspace.workspaceFolders.{supported, changeNotifications}`
(`ConnectionHandle::supports_workspace_folder_changes`); the acquire path
(`resolve_acquire`) checks the existing shared connection's capability and, if
it is `Ready` but incapable, logs once and falls back to the per-root key — so a
misconfigured opt-in degrades to per-root instances rather than wedging the
2nd+ root on a server that ignores `rootUri`. Marker-less documents are exempt
from that divert: they bring no marker root, so the missing capability never
blocks them and they ride the incapable shared connection rather than fork the
fallback — accepting that their didOpen may fall outside such a connection's
folders (their client-workspace announcement is capability-gated away;
out-of-workspace documents are LSP-legal, and diverting instead would re-split
the session-wide state this routing unifies). On capable connections the
complete client workspace (folder snapshot, or the bare rootUri when the
client sent no folders) is announced on their behalf. The shared connection's folder set
(`WorkspaceFolderSet`, shared with the reader task that answers
`workspace/workspaceFolders` pulls) grows as new roots join: on acquiring the
shared connection for a not-yet-known root, the pool CAS-inserts the root and
emits `workspace/didChangeWorkspaceFolders { added: [root] }` ahead of the
`didOpen` through the single-writer FIFO. The set is add-only; idle
removal/eviction is a separate follow-up.

**Known limitation:** per-root pooling multiplies process count with the number
of distinct roots opened, and there is no idle-eviction yet — see Consequences.
The shared-instance opt-in mitigates this for capable servers.

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
│  │           Each: ls-bridge-async-connection + ls-bridge-message-ordering                │ │
│  └────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

**Phase 1 Design:**
- **RequestRouter**: Routes by `languageId` to single server
- **Per-Connection Isolation**: Each downstream connection maintains its own actor (ls-bridge-message-ordering)
- **No Aggregation**: Single server per language, no fan-out

**Phase 3 Extension**: ResponseAggregator for multi-LS fan-out (see Future Extensions).

### Request ID Semantics

**Decision** (superseded by the implementation; amended with
bridge-routing-protocol): every downstream request id is
**bridge-minted** from the connection handle's own allocator
(`next_request_id`) — fan-out means several downstream requests can
serve one upstream id, so upstream ids cannot be reused directly.
Forwarded client traffic additionally records an upstream→downstream
cancellation mapping; bridge-initiated requests (`kakehashi/bridge/routing`)
have no upstream id and register with no such mapping. The original
Phase-1 sketch below reused the upstream id verbatim and is kept only as
history:

**Phase 1 Flow** (single server per language, historical):
```
Client (editor)          kakehashi           Downstream Server
     ├─ hover ID=42 ────→ Router ──────────────→ pyright (ID=42)
     ◀─ result ─────────────────────────────────◀
```

### Routing (Phase 1)

In Phase 1, routing resolves `languageId` → `server_name` via
configuration, then looks up the connection by `server_name`. The connection
is therefore keyed by **server name**, not by language — that is the key
difference from pure language-based routing: several languages sharing one
`server_name` share one process — `typescript` and `typescriptreact` both
resolve to `tsgo` and get the same connection.

(The full `ConnectionKey` is richer than a bare name — per-root pooling and
the shared-instance opt-in are above, under § Phase 1: Connection-Key
Routing. The name is the part
this comparison turns on.)

**Phase 3 Extension**: Multi-LS routing strategies — see Future Extensions.

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

**Malformed Initialize Capability Recovery:**

An object-shaped `result.capabilities` is decoded with field-level recovery.
Each top-level capability is validated once in isolation; invalid fields are
logged with their serde errors, while the valid raw fields are assembled and
decoded together into `ServerCapabilities`. Independently valid capabilities
remain available for routing, and the bridge completes the downstream
handshake without repeatedly traversing valid fields for every malformed one.

The recovery boundary follows the scope of the damaged state:

| Failure | Behavior |
|----------|----------|
| Malformed feature capability, such as `hoverProvider` | Drop that top-level capability, warn, continue |
| Several malformed feature capabilities | Drop and warn for each one, preserve the rest |
| Missing or null `capabilities` | Continue with empty capabilities for compatibility |
| Non-object `result` or non-object `capabilities` | Fail initialization; no reliable capability boundary exists |
| Invalid `positionEncoding` or legacy `offsetEncoding` | Fail initialization; position translation affects every bridged feature |

Rejecting the whole server for one independent feature maximizes protocol
strictness but sacrifices all otherwise usable functionality. Silently falling
back to empty capabilities preserves the process but hides the defect and loses
the same functionality. Field-level recovery accepts extra parsing complexity
to keep the decision stateless and the failure scope aligned with the malformed
advertisement.

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

**Connection Termination**: When a connection enters `Closed` state (graceful shutdown or respawn replacement), all document lifecycle entries for that downstream are discarded. A crash or panic enters pool-resident `Failed` instead — the handle stays addressable until a later acquire replaces it or cleanup/shutdown closes it (ls-bridge-message-ordering § Connection State Tracking) — and its document entries are discarded when that replacement or closure lands. A respawned connection starts with all documents in `Closed` (default) state, requiring fresh `didOpen` notifications.

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

**Implemented: `window/*` forwarding (#378, #852).** The reader task parses both
methods and applies the live global `features."window/logMessage".logLevel`
threshold before enqueue, so suppressed logs cannot consume the bounded queue
needed by allowed logs and unfiltered `window/showMessage`. The common
client-facing delivery boundary checks the same workspace policy again to close
live-update races and also shares it with kakehashi's own logs (`info` by
default). Both forwarded methods are
prefixed with `[kakehashi:<server>]` for
distinguishability and need
no coordinate translation. They reuse the `UpstreamNotification` decoupling
(reader task -> forwarding loop -> tower-lsp `Client`) but travel on a
**bounded** channel with drop-on-full, separate from the unbounded channel
carrying `workspace/diagnostic/refresh`: a log-flooding downstream server must
not grow memory without bound, and the forwarding loop's biased select drains
the refresh channel first so a `window/*` burst cannot starve diagnostics.
FIFO order is preserved within each channel. `$/progress` (#379) and
push-based `textDocument/publishDiagnostics` (#380) are forwarded too — the
latter into the diagnostics cache per push-propagation-diagnostic-forwarding
rather than passed straight through.

### Cancellation Propagation

See ls-bridge-message-ordering § Cancellation Forwarding for single-connection cancellation semantics.

**Multi-Connection Coordination (Phase 3)**: Router forwards `$/cancelRequest` to all connections that received the original fan-out request.

## Future Extensions

### Phase 3: Multi-Server Backpressure Coordination

When routing notifications to multiple servers for the same language (Phase 3), if one server's queue is full, notifications are handled independently per server.

**Decision**: Accept state divergence under extreme backpressure (non-atomic broadcast).

```
Router sends didSave to pyright + ruff (both handle Python):
├─ pyright: queue full → DROP (per ls-bridge-message-ordering)
└─ ruff: queue OK → FORWARD

Result: State divergence (recoverable via next didChange)
```

**Rationale**: Servers already handle being attached at arbitrary points in a document's lifetime.

### Phase 3: Response Aggregation Strategies

> **Note**: The multi-server aggregation this section introduced as "Phase 3"
> has since **shipped** — dispatch fans out to every selected server, and the
> strategies and allowlist are owned by cross-layer-aggregation and
> aggregation-priorities-wildcard. What remains unimplemented is the
> per-request timeout tier the stability rules below assume
> (ls-bridge-timeout-hierarchy places Tier 1 in Phase 3). Read the rules as
> target state and the capability as current.

For fan-out **requests** (with `id`), aggregation is configured per method.
The strategies and the priority-ordered allowlist that selects participants
are owned by cross-layer-aggregation and aggregation-priorities-wildcard —
that is where the shipped names and semantics live. This section adds only
the *stability* rules a multi-server fan-out needs on top of them.

**Aggregation Stability Rules:**
- **Per-request timeout conditions**: the timeout applies **only when n ≥ 2
  downstream servers participate** in aggregating one document request
  (default: 5s explicit, 2s incremental), whatever strategy selected them.
  A routing-provider fan-out is not such an aggregation: it stays exempt,
  bounded solely by its own routing deadline (bridge-routing-protocol,
  ls-bridge-timeout-hierarchy). A single participant needs no aggregation
  bound — nothing is being waited *together* — and the per-downstream
  response cap already bounds its individual wait; liveness separately
  detects connection silence
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

```toml
# Phase 3: several servers for one injected language, with per-method
# aggregation. `priorities` is an ordered allowlist; a server absent from
# the list does not run (aggregation-priorities-wildcard).
[languages.markdown.bridge.python.aggregation._]
priorities = ["ruff", "pyright"]   # ruff first where capabilities overlap

[languages.markdown.bridge.python.aggregation."textDocument/completion"]
strategy = "concatenated"          # safe: candidates, the user selects one

[languages.markdown.bridge.python.aggregation."textDocument/codeAction"]
strategy = "concatenated"          # safe: proposals, the user executes one

# hover, definition, rename: left to the default `preferred` dispatch — one
#   server answers, so overlapping WorkspaceEdits cannot arise.
# formatting: see concatenated-formatting-pipeline (sequential pipeline,
#   implemented; user-issued textDocument/rangeFormatting unaffected)

[languageServers.pyright]
cmd = ["pyright-langserver", "--stdio"]
languages = ["python"]

[languageServers.ruff]
cmd = ["ruff", "server"]
languages = ["python"]             # same language as pyright — this is the fan-out
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
- The Phase 1 architecture was built to admit multi-LS extension, which has since shipped
- Single-server configurations continue to work unchanged

### Negative

**Single Server Limitation (Phase 1)** — *historical; multi-server fan-out
has since shipped:*
- Could not use multiple servers for the same language (e.g. pyright + ruff) until Phase 3

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

## Related Decisions

- **[language-server-bridge](language-server-bridge.md)**: Core LSP bridge architecture (1:1 pattern)
  - ls-bridge-server-pool-coordination extends to 1:N (one client → multiple servers per language)
- **[language-server-bridge-request-strategies](language-server-bridge-request-strategies.md)**: Per-method bridge strategies
  - Per-method strategies remain valid for single-server routing
- **[ls-bridge-async-connection](ls-bridge-async-connection.md)**: Async Bridge Connection (single-server I/O)
  - Provides async I/O patterns enabling parallel server management
- **[ls-bridge-message-ordering](ls-bridge-message-ordering.md)**: Message Ordering
  - Handles single-server ordering; ls-bridge-server-pool-coordination coordinates multiple servers
- **[ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md)**: Graceful Shutdown
  - Defines shutdown coordination for multiple concurrent connections
  - Router sends one `Teardown` message to the lifecycle actor, which launches per-connection shutdown sub-tasks; ls-bridge-graceful-shutdown specifies the per-connection sequence
- **[bridge-routing-protocol](bridge-routing-protocol.md)**: Downstream routing delegation
  - A routing provider's `workspaceFolders` answer overrides the marker-walk root resolution
  - `forceStart` spawns eagerly — the `#shared` key with the primary-root seed for `preferSharedInstance` servers, the marker-less fallback shape otherwise

## Amendment History

- **2026-07-18**: Added field-level recovery for malformed downstream
  initialize capabilities (#860). Structurally unusable envelopes and global
  position-encoding violations still fail initialization; independent malformed
  feature fields are warned and dropped so valid routing capabilities survive.
- **2026-07-13**: Added one workspace-wide `window/logMessage` severity policy
  for downstream-forwarded and kakehashi-originated messages (#852). The
  default is `info`; `window/showMessage` remains unaffected.
- **2026-08-11**: Marker-less documents of a `preferSharedInstance` server now
  join the shared instance instead of the client-root fallback. The exclusion
  dated from #391's framing of shared-membership as "roots that join via
  `didChangeWorkspaceFolders`" — but announcing is what new ROOTS need, not
  what admission needs, and the fallback fork split session-wide downstream
  state: an editor's `untitled:` scratch document landed on a second server
  process that never saw the documents on the shared one (observed as a
  completion corpus missing every open buffer's words). Marker-less documents
  are also exempt from the incapable-shared divert, since they bring no marker
  root the capability would be needed for (their client-workspace announcement
  is capability-gated away there — the possibly out-of-workspace `didOpen` is
  the accepted residual recorded above). Non-opted-in servers keep the client-root
  fallback for marker-less documents. Known consequence: a shared connection
  spawned by a marker-less FIRST acquisition seeds its folder set and
  initialize handshake with the full client snapshot of that moment, and
  `apply_workspace_folder_change` deliberately does not keep that seed current
  (the folder set has one writer after spawn — acquisitions; forwarding client
  changes would need per-folder provenance). Served-root proof for the
  incapable divert is a separate, tiered fact (`incapable_shared_serves`):
  initialize-listed folders count for a server that declared
  `workspaceFolders.supported` (only change notifications missing), while a
  server with no folder support at all is only ever proven for its recorded
  spawn root (compared by filesystem path, so trailing-slash or
  percent-encoding differences between a client root string and a marker
  walk's URL cannot fake a mismatch).
- **2026-07-01**: Renamed the `languageServers.*.rootMarkers` config key to
  `workspaceMarkers` (aligning with the LSP spec's `workspaceFolders`); the old
  `rootMarkers` is accepted as a deprecated serde alias for backward
  compatibility. The marker walk itself and the pool-keying behavior are
  unchanged; only the config key name moved. Earlier amendment entries below
  still reference the pre-rename key as the historical record of their date.
- **2026-06-20**: Added the per-server `preferSharedInstance` opt-in (#391): capability-gated routing to one `ConnectionKey::shared` connection across roots, a mutable per-connection `WorkspaceFolderSet`, and `workspace/didChangeWorkspaceFolders` emission ahead of `didOpen` for newly joined roots. Default stays per-root (#382); incapable servers (no `workspace.workspaceFolders.{supported, changeNotifications}`) log once and fall back to per-root.
- **2026-06-20**: Extended pool keying from `server_name` to `(server_name, resolved workspace root)` (`ConnectionKey`) for multi-root monorepos (#382). The root is resolved from the triggering document's `rootMarkers` walk, shared with the spawn handshake, and stored on the connection handle so all per-connection state routes via `handle.key()`. Documents under different marker roots get separate downstream processes; marker-less documents share the client-root fallback. Follow-up: idle-eviction policy to bound process growth.
- **2026-01-24**: Changed from language-based to server-name-based pool keying to enable process sharing for related languages (e.g., ts/tsx sharing tsgo). Connection pool is now keyed by `server_name` instead of `languageId`, with configuration resolving `language` → `server_name`.
- **2026-01-07**: Merged Amendment 002 - Simplified ID namespace by using upstream request IDs directly (no transformation), replaced `pending_correlations` with `pending_responses`
- **2026-01-06**: Merged Amendment 001 - Updated partial results to use LSP-native fields (isIncomplete), clarified $/cancelRequest semantics, added response guarantees for cancelled requests
- **2026-08-12**: Applied the contract/invariant/mechanism discipline (template.md) - retired two drifted sketches that read as guarantees: the `FirstWins`/`MergeAll`/`Ranked` strategy enum (the shipped strategies are `Preferred`/`Concatenated`, owned by cross-layer-aggregation and aggregation-priorities-wildcard) and the Phase 1 `route_request` snippet; the Phase 3 stability rules now turn on participant count rather than the never-shipped `SingleByCapability`/`FanOut` names; the Phase 3 configuration example was rewritten against the shipped TOML schema (`bridge.<lang>.aggregation`, `priorities`, `strategy`), having drifted to a YAML shape with `priority`/`dedup_key`/`single_by_capability` that never existed
