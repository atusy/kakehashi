# Bridge Client Control Protocol

**Related Decisions**:
- [node-reference-protocol](node-reference-protocol.md) — precedent for a handle-based `kakehashi/*` custom-method family
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the `ConnectionKey` identity this protocol exposes
- [ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md) — the shutdown handshake `stop` reuses
- [respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md) — the derived re-open `restart` relies on
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — the cancellation forwarding that the pass-through request reuses
- [ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md) — the timeout tiers `stop`, `restart`, and pass-through requests interact with

## Context

The bridge manages downstream language servers invisibly. The editor-side
client can observe their *effects* (diagnostics, hover results) but has no way
to observe or control the servers themselves: which downstream processes exist,
which documents each one holds, what a server announced at initialization, or —
most importantly — how to stop or restart a wedged server without restarting
kakehashi itself.

It also has no escape hatch. When a downstream server supports a capability the
bridge does not yet translate, the only options are to wait for a bridge
feature or to run the server directly, losing the injection machinery.

node-reference-protocol opened the syntax tree as a platform surface; this
protocol does the same for the connection pool. The primitives reduce to:
**enumerate clients**, **inspect one** (documents, server info, workspace
folders), **control one** (stop, restart), and **talk to one directly**
(request, notify).

## Decision

Introduce a custom-method family under `kakehashi/bridge/client`, addressing
downstream connections by an opaque id derived from the pool's `ConnectionKey`.

### Method Catalog

| Method | Kind | Input | Output |
|---|---|---|---|
| `kakehashi/bridge/client` | request | `{ textDocument?, name? }` | `Client[]` |
| `kakehashi/bridge/client/request` | request | `{ id, method, params? }` | `ForwardResult` |
| `kakehashi/bridge/client/notify` | notification | `{ id, method, params? }` | — |
| `kakehashi/bridge/client/documents` | request | `{ id, documentSelector? }` | `OpenDocument[]` |
| `kakehashi/bridge/client/serverInfo` | request | `{ id }` | `ServerInfo \| null` |
| `kakehashi/bridge/client/serverCapabilities` | request | `{ id }` | `{ static: ServerCapabilities, dynamic: Registration[] }` |
| `kakehashi/bridge/client/workspaceFolders` | request | `{ id }` | `WorkspaceFolder[]` |
| `kakehashi/bridge/client/stop` | request | `{ id }` | `null` |
| `kakehashi/bridge/client/restart` | request | `{ id }` | `null` |

The methods are announced in the initialize result as
`capabilities.experimental.kakehashi.bridgeClient: true`, beside the existing
`wrappedDidChangeConfigurationSettings` flag, so clients can feature-detect
before use. This sets a new precedent rather than following one: the existing
`kakehashi/node*` and `kakehashi/captures/*` families are announced nowhere.

### Client Identity: the `ConnectionKey` `Display` String

```typescript
type Client = {
  name: string;    // config server name (the `languageServers` map key)
  id: string;      // opaque handle; currently the ConnectionKey Display string
  status: "starting" | "running" | "stopping" | "stopped" | "failed";
};
```

The `id` is the `Display` rendering of the pool's `ConnectionKey`
(`tsgo@file:///repo/a`, `lua`, `tsgo#shared` — see
ls-bridge-server-pool-coordination). Three properties motivate this choice:

1. **Slot identity, not process identity.** A `ConnectionKey` names a
   `(server, root)` slot; the process behind it may be replaced by `restart` or
   crash-respawn while the key stays fixed. A held id therefore survives
   restarts, matching the protocol's `restart → null` contract (no new id to
   return).
2. **Already threaded everywhere.** Every per-connection map in the pool —
   connections, document versions, host-document sync state, the respawn
   ARM/CLAIM set — is keyed by `ConnectionKey`, alone or inside a composite
   key, so id-based routing needs no new registry.
3. **Human-legible.** The rendering carries the root, so two same-name clients
   in a monorepo are distinguishable at a glance in the enumeration result
   without an extra round trip.

**The id is contractually opaque.** Clients must obtain ids from the
enumeration request and echo them back verbatim; the server resolves an id by
exact string comparison against current keys and never parses it. The rendering
may change between kakehashi versions.

**Server-name validation.** The `Display` rendering is injective only if
server names cannot collide with its markers: a server literally named
`a#shared` would render identically to the shared-instance key of server `a`.
Configuration loading therefore rejects `languageServers` names containing
`@` or `#`.

**`status`** mirrors the connection state machine
(ls-bridge-graceful-shutdown): `starting` = `Initializing` **or** a
pre-handle `Spawning` intent entry (an ordinary acquire's spawn commit
before the child's handshake begins), `running` =
`Ready`, `stopping` = `Closing` **or** a termination-pending record whose
process termination is not yet confirmed (its handle may already be
`Closed` or gone), `failed` = `Failed`, and `stopped` =
pinned until an explicit `restart` — by a user's `stop`, or by the
fenced retry tombstone a failed `restart` leaves (the enumeration does
not distinguish who pinned it; both revive the same way — see below). Stopped slots **are included** in
the enumeration — they are absent from the live pool, so the enumeration is
the only way to recover their id for a later `restart`. `Failed` connections
stay pool-resident until a later acquire replaces them, so `failed` is
observable, if transient.

### Enumeration: `kakehashi/bridge/client`

Both parameters are optional and compose as AND:

- `textDocument?: TextDocumentIdentifier` — only clients that serve this
  document: the document (or one of its injections) bridges to the server
  *and* resolves to this connection's root. A document kakehashi does not
  have open matches nothing — the parameter is a filter, not a lookup.
- `name?: string` — only clients spawned from this `languageServers` entry.

With neither parameter, all clients (including stopped slots) are returned.

### Pass-Through: `request` and `notify`

`params: { id, method, params? }` — the inner `method`/`params` are forwarded
to the downstream connection **verbatim**: no URI rewriting, no position
mapping, no capability filtering, no document-lifecycle bookkeeping. This is
the deliberate boundary between the bridge's managed domain and the caller's
self-responsibility domain:

- The bridge guarantees its own *records* stay coherent: pass-through
  traffic never mutates the bridge's document registry, virtual documents,
  or capability records.
- The bridge does **not** guarantee the bridge↔downstream *agreement* stays
  coherent. A pass-through `textDocument/didOpen` creates a downstream
  document the bridge does not know about; a pass-through
  `textDocument/didClose` on a bridge-managed URI makes the downstream
  forget a document the bridge still believes is open, degrading every
  bridge-managed feature for it until it is re-opened. Both are the caller's
  responsibility; a request naming a document the bridge never opened is
  forwarded anyway (this holds after `restart` too — the derived re-open
  restores only bridge-managed documents).

**Denied methods.** Five methods would not merely extend the downstream's
state — they would corrupt the bridge's own protocol state machine — and are
rejected: `initialize`, `initialized`, `shutdown`, `exit`, and
`$/cancelRequest` (cancellation is expressed by cancelling the outer
request — below). A denied `request` fails with
`data.reason: "methodDenied"`; a denied `notify` is dropped and logged
(notifications have no response channel).

**Message-kind and params validity.** The bridge cannot know whether an
arbitrary method name is a request or a notification, so it does not
validate the kind: `request` always emits a downstream request, `notify`
always emits a downstream notification. A `request` carrying a
notification-kind method may never receive a downstream response — though a
server seeing the unexpected `id` may answer with a result or an error —
and can remain pending until cancelled; a `notify` carrying a request-kind
method is a well-formed JSON-RPC notification the server will most likely
ignore. Both are the caller's responsibility. The inner `params` must be an
object, an array, or absent — the only shapes JSON-RPC permits — and
anything else is rejected before forwarding: `request` fails with
`InvalidParams` (`-32602`); `notify`, having no response channel, drops and
logs it.

**Response envelope.** Except on cancellation or connection loss (below),
the outer request succeeds whenever forwarding succeeded, and its result
wraps the downstream's answer:

```typescript
type ForwardResult =
  | { result: LSPAny }         // includes result: null — always serialized
  | { error: ResponseError };  // the downstream's error, verbatim
```

Exactly one of the two keys is present, and on the success branch `result`
is always emitted — explicitly `null` when the downstream answered `null`,
a routine LSP outcome (hover misses, empty definitions) — so an empty
envelope can never masquerade as a null result. The guarantee is scoped to
valid JSON-RPC responses: a downstream answer carrying both keys, neither
key, or a malformed error object fails the outer request with
`data.reason: "malformedResponse"` instead of being relayed — provided the
envelope still carries the matching bridge-minted id. An invalid message
that cannot be correlated to its pending entry is dropped and logged, and
the outer request stays pending until cancellation or connection loss.

The JSON-RPC framing fields (`jsonrpc`, `id`) are stripped: the downstream
`id` is a bridge-internal request id, unrelated to the caller's, and echoing
it would only invite confusion. Outer-level errors (unknown id, stopped
client, denied method) use the error model below, so callers can distinguish
"the bridge could not forward" from "the server answered with an error".

**Cancellation, timeouts, and connection loss.** A `$/cancelRequest` for the
*outer* request id is forwarded to the downstream as a cancel of the inner
request (ls-bridge-message-ordering § Cancellation Forwarding; mechanically,
the handler remembers the downstream id it minted and calls
`forward_cancel_downstream`). The outer request then fails with
`RequestCancelled` (`-32800`); the pending entry is retired atomically with
that error, so a downstream answer arriving later has nowhere to land and
is dropped and logged. This is a deliberate exception to
ls-bridge-message-ordering's forward-the-late-response rule — that rule
serves callers still waiting for a result, and this caller has already
abandoned it. Forwarding the cancel serves the *downstream*, not the
caller: the outer request is released by `RequestCancelled` regardless, and
the forward stops the abandoned computation. Cancellation still matters to
the caller because the outer request carries **no bridge-imposed timeout**:
the bridge cannot know the expected latency of an arbitrary method, so the
caller decides how long a pass-through may take, and cancellation is the
only way out. For the same
reason pass-through requests are excluded from Tier-2 liveness accounting
(ls-bridge-timeout-hierarchy): a deliberately slow custom request is not
evidence of a hung server and must not drive a healthy connection to
`Failed`. `notify`, having no response, is not cancellable. If the
connection dies after forwarding (crash, disposal), the outer request fails
with `data.reason: "connectionLost"` rather than fabricating a downstream
error. Because there is no bridge-imposed timeout and no liveness
accounting, pending pass-throughs need their own bound: a **per-connection
in-flight limit** caps concurrent pass-through requests, each slot (and
its downstream-id mapping) released on **every terminal outcome of the
outer request** — downstream response, outer cancellation, connection
loss, or `forwardFailed` (a failed forward never retains the slot, so
repeated writer-full rejections cannot leak the limit away); a request
beyond the limit fails fast with `data.reason: "passThroughLimit"` — a
downstream that accepts requests but never answers can therefore pin at
most the limit, never unbounded outer requests and id mappings.

**Pass-through is caller→downstream only.** Server-initiated traffic that a
pass-through call provokes — `$/progress`, `workspace/configuration`,
`workspace/applyEdit`, any server→client request — flows through the
bridge's normal inbound handling under bridge policy, exactly as if a
bridged request had provoked it. In particular, the bridge does not rewrite, reserve, or namespace a
caller-minted `workDoneToken` riding the inner params: progress for a token
the bridge does not recognize is dropped, and a collision misroutes it.
There are two collision domains — the bridge's own minted client-progress
tokens (predictable strings) and the connection's server-declared progress
registry, whose live tokens the caller cannot enumerate — so deterministic
collision avoidance is impossible; high-entropy random tokens make it
negligible, not zero. Callers must not rely on progress or server-request
round trips through the escape hatch.

### Error Model

Bridge-level failures use `RequestFailed` (`-32803`) with a machine-readable
discriminator in `data`. The `bridge/client:` message prefix marks
control-protocol failures; bridged LSP requests keep their existing
`bridge:` messages from ls-bridge-graceful-shutdown:

```json
{ "code": -32803, "message": "bridge/client: <human summary>",
  "data": { "reason": "unknownClient" } }
```

| `data.reason` | Meaning |
|---|---|
| `unknownClient` | id matches no current slot |
| `clientNotReady` | slot is `starting`, `stopping`, or `failed`; `data.status` carries which |
| `clientStopped` | slot pinned until restart — by a user's `stop` or a failed restart's retry tombstone; `restart` revives it |
| `clientRestarting` | slot is mid-`restart`; retry after it returns |
| `restartFailed` | the `restart` failed — in its stop phase before respawn, at spawn (missing or unspawnable command), during initialization, or at completion, when the ownership check found the replacement displaced (e.g. by a reload); the message names the cause |
| `stopFailed` | a `stop` failed to reach a verified `Closed` — abnormal sub-task outcome (panic, cancellation) or termination unconfirmed at the per-slot deadline |
| `forwardFailed` | the inner message could not be handed to the connection (bounded writer full, send error); nothing was forwarded |
| `connectionLost` | the connection died after the inner request was forwarded |
| `malformedResponse` | the downstream answered with an invalid JSON-RPC response |
| `methodDenied` | inner method is on the deny list |
| `passThroughLimit` | the per-connection in-flight pass-through limit is reached; retry after earlier requests settle |

The bridge never parks a call to wait for a status change: pass-through and
inspection requests against a slot that is not `running` fail fast with the
reasons above (`documents` answering `[]` for a `stopped` slot is the one
deliberate exception). `stop` and `restart` do wait — but only on the
shutdown and initialization handshakes they themselves initiate, never on a
status change someone else must cause.

`notify` never errors — including for malformed inner `params`, which are
dropped and logged rather than rejected (`InvalidParams` applies to
`request` only). It is forwarded only while the slot is `running`: during
`starting` a pass-through notification could land between `initialize` and
`initialized`, which LSP forbids, so it is dropped there like everywhere
else — silently, logged at debug level — during
`starting`/`stopping`/`stopped`/`failed`, the restart window, and for
unknown ids.

### Inspection: `documents`, `serverInfo`, `workspaceFolders`

```typescript
type OpenDocument = {
  uri: string;         // as the downstream sees it (virtual URI for injections)
  languageId: string;
  version: number;     // current tracked version for this connection
  hostUri?: string;    // host document a bridge-minted virtual document derives from
};
```

- `documents` returns the bridge-managed open documents of the connection —
  virtual documents included, distinguished by `hostUri`. Documents opened
  via pass-through are invisible here (outside the managed domain, by the
  boundary above). `documentSelector?: DocumentSelector | null` filters against the
  downstream-facing `uri`/`languageId`, accepting the text-document filter
  forms (`language`/`scheme`/`pattern`); `pattern` is a **plain glob
  string** — the vendored filter type declares a string pattern, so an
  object-valued 3.18 `RelativePattern` would not even deserialize and is
  rejected as `InvalidParams`, exactly like notebook filters (which need
  notebook context the bridge does not hold for virtual documents).
  Omitted or `null` returns all. A `stopped` slot holds
  nothing open, so it answers `[]`.
- `serverInfo` returns the `serverInfo` field of the downstream's initialize
  result. `null` means the downstream provided no usable value — omitted,
  JSON `null`, or malformed and dropped under the initialize parser's
  existing tolerance policy (malformed metadata never fails
  initialization; only capabilities are load-bearing). A slot that is not
  `running` fails with `clientStopped`/`clientNotReady` instead, so "no
  usable `serverInfo`" and "no live connection" never blur.
- `serverCapabilities` returns as `static` the bridge-retained,
  normalized capabilities — the typed subset the initialize parser kept
  (malformed fields dropped under its tolerance policy), deliberately
  *not* a byte-for-byte reproduction of the announcement — plus the
  bridge's record of currently active dynamic registrations
  (`client/registerCapability`, minus later unregistrations) as
  `dynamic`. This is what makes the pass-through escape hatch
  discoverable: the protocol's own motivation — reaching capabilities
  the bridge does not yet translate — requires the caller to *see*
  those capabilities, and `serverInfo` alone names the server without
  them. Like `serverInfo`, it fails for slots that are not `running`.
- `workspaceFolders` returns the folder set the bridge maintains for the
  connection, as `WorkspaceFolder[]`. Every connection carries a
  `WorkspaceFolderSet` seeded at spawn (it grows only for shared instances),
  so there is no null case; a workspace-less client-fallback connection
  answers `[]`. This is a dedicated request — rather than a `rootUri` field
  on `Client` — because a `preferSharedInstance` connection serves *many*
  folders and a scalar field cannot represent that. Like `serverInfo`, it
  fails for slots that are not `running`.

### `stop`: Graceful, and Pinned Until Explicit Restart

`stop` runs the graceful shutdown sequence (ls-bridge-graceful-shutdown):
`Closing` → LSP `shutdown`/`exit` handshake → `Closed`. The handshake is
bounded by a **per-connection shutdown timeout**, which is new state: today
the per-connection path deliberately carries no timeout of its own — only the
pool-wide `GlobalShutdownTimeout` bounds it, via the teardown-only
`force_kill_all` — so a single-slot `stop` against a wedged server would
otherwise wait forever, and a wedged server is precisely this method's
motivating case. On expiry the forced escalation applies (SIGTERM → SIGKILL
on Unix; immediate kill on Windows). The timeout's duration is
implementation-defined with a documented default in the same 5–15 s
class as the global teardown ceiling (a configuration knob can follow if
needed), and it is **one deadline** covering queue drain, the shutdown
handshake, and escalation initiation; termination *confirmation* is
deliberately outside it — a child unconfirmed at the deadline takes the
termination-pending path below, so `stop` latency stays predictable
while confirmation stays honest. Operations follow that decision's disposal policy, with the timeout being
this per-connection one: the already-accepted order queue drains ahead of
`shutdown` (graceful path only — a forced termination abandons the
remainder, which then fails like pending work), pending responses fail at
connection closure or the timeout — whichever comes first — and newly
arriving operations are rejected immediately. The result `null` is returned when
the slot reaches `Closed`, whichever path got it there. `stop` on a
`starting` slot is legal and aborts initialization with **no LSP message
at all**: the spec forbids the client every additional request *and*
notification — `exit` included — until the initialize response arrives, so
the process is terminated directly (the forced escalation, minus the
handshake). The race is decided by the lifecycle transition, not by
response receipt: `Initializing → Closing` always means direct
termination, and only a slot that already committed `Initializing → Ready`
— initialize response processed *and* `initialized` enqueued — takes the
normal `shutdown` → `exit` sequence, so `shutdown` can never jump the
queue ahead of `initialized`. ls-bridge-graceful-shutdown's
initialization-abort rule is amended alongside this decision — its
earlier revision sent `exit` in that window, which the same LSP ordering
forbids. Two invariants make that arbitration sound:
every handshake terminal transition — `Ready`, and `Failed` from error,
timeout, or task failure alike — commits **conditionally from
`Initializing`**, so once `Closing` wins nothing overwrites it (today the
timeout and task-failure paths write `Failed` unconditionally and the
error path is check-then-write); and the `initialized` enqueue commits
**atomically with the `Ready` transition**, while the direct-abort path
does not drain the queue — a losing handshake can neither enqueue
`initialized` after `Closing` won nor have the abort flush it downstream. `stop` on a `failed` slot skips the LSP
handshake — the `Failed → Closed` bypass ls-bridge-graceful-shutdown already
defines — cleans up the process, and records the stopped entry: pinning a
repeatedly failing slot before the next acquire respawns it is a
first-class use of this method, not an edge case. `stop` on a pre-handle
`Spawning` entry **settles the intent**: the entry is marked settling
and retained, any handle its escrow receives is killed and reaped, and
the outcome branches **on producer termination first**, which makes the
partition exhaustive and exclusive. Producer terminated: either no
child was ever created or its reap verified — the config-revalidated
tombstone installs and `stop` answers `null` — or a known child's reap
is unconfirmed, which answers `stopFailed` and converts to the ordinary
termination-pending record (the known-handle path below). Producer
still running at the deadline: `stop` answers `stopFailed` and leaves
the fenced open-escrow cleanup record — no tombstone yet, the eventual
child killed and reaped on arrival (ls-bridge-graceful-shutdown
§ Lifecycle Actor). `restart` on a `Spawning` slot is that same
settlement followed by its ordinary respawn sequence, respawning only
after a verified settlement and answering `restartFailed` with the
corresponding record short of one.

What is new is the **stopped set**: the pool records the `ConnectionKey`
as pinned (a user's `stop`, or a failed restart's retry tombstone), and
the normal routing path consults it — a `didOpen` (or
any acquire) that resolves to a stopped key does **not** spawn. The slot's
features stay dark until a `kakehashi/bridge/client/restart` clears the entry.
This is a deliberate behavior change to the normal path: without it, the next
keystroke in a matching document would resurrect the server and `stop` would
be advisory. `stop` on an already-stopped slot returns `null` (idempotent).

Three lifecycle rules keep the set coherent, and all three are
consequences of the pool's **lifecycle actor**
(ls-bridge-graceful-shutdown § Lifecycle Actor), which owns every
lifecycle transition — stop, restart, spawn commit, teardown, tombstone
and termination-pending bookkeeping — and processes them one message at a
time:

- **Acquires cannot race a stop.** An existing `Ready` connection is used
  lock-free, but *creating* one is a spawn-commit message: the actor
  checks the stopped set — and any in-flight control operation or
  teardown seal — in-queue, so an acquire can never observe "not
  stopped", lose the race to a committing `stop`, and spawn beside a
  tombstone. The two orders are the only orders.
- **Control calls are single-flight per key** — as a consequence of the
  queue, not of a guard: a second `stop`/`restart` for a key with an
  operation in flight is simply the next message, answered from the key's
  current state (`clientRestarting` during a restart; `clientNotReady`
  with `data.status: "stopping"` during a stop) instead of interleaving
  into a state where a slot is simultaneously live and stopped.
- **The set is process-lifetime and config-checked.** A configuration
  reload drops stopped entries whose server name no longer exists in
  `languageServers` — their ids then resolve as `unknownClient` — while
  entries whose server survives the reload stay stopped. Reload's
  connection purges and the tombstone installs are both actor
  transitions, mutually ordered by the queue: a `stop` committing after a
  reload deleted its server installs no entry (the slot is already gone
  and its id resolves `unknownClient`), never a stale one. Nothing is
  persisted across kakehashi restarts.

### `restart`: Same Key, Current Config, Derived Re-Open

`restart` = the `stop` *sequence* above (with its per-status shortcuts; a
no-op if the slot is already stopped) — but **without installing a
stopped entry**: the tombstone is `stop`'s artifact, and during a restart
the key's in-flight operation state stands in for it, so no interval
exists in which the slot reads `stopped` or errors `clientStopped`
mid-restart. A pre-existing entry from an earlier `stop` is cleared in
the same actor transition that begins the restart. Then respawn the
**same** `ConnectionKey` under the configuration current at that moment —
"that moment" being the replacement-insertion transition: the actor
re-reads the configuration generation there, and because reload's
publication into the pool is itself an actor transition, no window exists
where a newer generation publishes between validation and insertion. The
outer request resolves with `null` only at the **completion transition**,
where the actor verifies the exact replacement is still pool-resident and
`Ready` — reaching `Ready` is necessary, not sufficient — or fails (error
`restartFailed`), bounded by the existing initialization timeout
(ls-bridge-timeout-hierarchy). `restartFailed` covers every failure
shape — a spawn that dies before a handle exists (missing binary, invalid
or unspawnable current configuration) as much as a handle that reaches
`Failed` — with the underlying cause in the error message. The slot's id
must survive either way: a failure that reached `Failed` leaves the
handle pool-resident (enumerable as `failed`, healed by the ordinary
acquire-driven respawn), while a failure that never produced a handle
**re-installs the stopped entry** — the id stays enumerable as `stopped`
and a later `restart` retries — under the same config-revalidation rule
as any tombstone install (a server deleted mid-restart dissolves the id
into `unknownClient` instead). Recovery is decided by **pool ownership at
the completion transition**, which also covers a replacement a reload
removed while still `Initializing`: a still-configured id never silently
disappears. That is the **ownership-at-completion rule**, stated once: a
key whose replacement is not pool-resident and `Ready` at the completion
transition re-installs the fenced tombstone if its server is still
configured, and dissolves into `unknownClient` if a reload deleted it.

Neither `stop` nor `restart` is abortable mid-mutation: a
`$/cancelRequest` for the outer request may fail it with
`RequestCancelled`, but the operation runs to completion as the key's
state machine inside the lifecycle actor — a dropped handler drops only
its reply channel, never the operation. Pool-wide shutdown does not wait
behind them either: `Teardown` is a message on the same queue, ordered
against every control transition by construction; the teardown state
machine advances or settles in-flight operations, awaits or abandons
their sub-tasks (handshakes, kills, waits) within the deadline, and the
escalation reserve covers every process the actor's state records. A
wedged per-slot `stop` can therefore neither stall teardown, nor outlive
it, nor mutate pool state after cleanup; the outer control request then
settles `stopFailed`/`restartFailed` per the settlement rules below —
**answering the outer request only while its reply is still live**: a
caller already released by `RequestCancelled` sees nothing more, and the
settlement is then internal lifecycle bookkeeping (deadline shape:
ls-bridge-timeout-hierarchy § Per-Slot Control Shutdown).

The abort-safety story is short because the actor makes it so: **all
actor-owned lifecycle state effects happen inside the actor's message
handling, which is serialized and non-suspending per message** (the one
boundary: per-connection handshake terminal compare-transitions stay
outside the actor, per ls-bridge-graceful-shutdown's stated boundary). Sub-tasks carry only their job parameters and borrowed resources (a
shared `Arc` of the process handle, I/O endpoints a handshake transition
lends) — the authoritative process handle and the reply sender stay in
the actor's state entry, so a panicked or stale sub-task can lose
neither; the writer's stdin handoff is likewise consumed inside such a
sub-task, and a completion message reports only the outcome and its
generation — never a resource, so nothing can strand in a channel
buffer — and a sub-task that dies (panic, cancellation, crash) is
observed via the actor's `JoinSet` and settled like any other completion:
the actor applies the ownership-at-completion rule to its own state
entry, settles the tombstone/ARM/replacement records, and **settles the
outer result exactly once — answering only while the reply is still
live** (a cancelled caller was already released and sees nothing more). `stop` answers `null` when the slot
verifiably reached `Closed` — with its tombstone installed, or with the
tombstone legitimately omitted because a reload deleted the server
(closure is what `stop` promises; the tombstone is bookkeeping) — and
otherwise `RequestFailed` with `data.reason: "stopFailed"`; `restart`
answers `null` only after the verified-Ready completion transition and
`restartFailed` short of it. A panicking sub-task can therefore never
leave the caller pending, kill a committed replacement, or misreport
success. One implementation precondition transfers unchanged from the
earlier lock-based design: purge paths that today await while holding
`connections` must become non-suspending actor transitions — any awaiting
work moves into a sub-task that reports back — before `restart` may adopt
them.

Process termination has **exactly one owner: the actor.** Confirmation
means exactly that `Child::wait` returned `Ok(ExitStatus)` — the one
primitive that both observes and reaps; SIGKILL delivery proves nothing,
and a `wait` `Err` retries with logged, bounded backoff, staying fenced
however long it fails (operator-visible as `stopping`, never a silent
promotion or a hot loop). A child whose termination is unconfirmed at the
per-slot deadline never settles as closed: `stop` settles `stopFailed`,
`restart` settles `restartFailed`, and the key converts to a
**termination-pending record** in the actor's state that retains the kill
handle, enumerates as `stopping`, blocks acquires and further control
calls (`clientNotReady`), survives reload purges, and converts to the
fenced retry tombstone only when `wait` returns `Ok`; a background
sub-task drives the pending `wait`s. A replacement must never spawn while
the old process may still hold locks or sockets, and a later `restart`
must never clear a tombstone that does not exist yet. This
never-`Closed`-while-unconfirmed rule is a **steady-state** invariant;
global teardown's final deadline disposes by mode instead — the
process-exit path logs and abandons the child to the OS, while a
shutdown-request teardown that leaves the server alive keeps the records
and their waits (ls-bridge-graceful-shutdown § Unconfirmed termination).

- **Process-level configuration applies.** `command`, `args`,
  `initializationOptions`, settings — whatever the config says *now* is what
  the replacement is spawned with.
- **No re-key.** Key-*defining* configuration (`workspaceMarkers`,
  `preferSharedInstance`) does not re-route the existing slot: the replacement
  is spawned under the identical key, and rooting changes take effect only for
  newly routed documents. This is what keeps held ids stable; the alternative
  is rejected below.
- **Document restoration is derived, not remembered.** The respawn uses the
  existing ARM/CLAIM mechanism, with one addition: the acquire path ARMs
  only when it replaces a live entry, and after `stop` the entry is already
  gone, so `restart` **explicitly ARMs the key** after its purge phase and
  before the respawn — otherwise the replacement's handshake would find
  nothing to claim. The replacement then re-opens whichever currently open
  documents belong to it, per respawn-reopen-derives-its-targets. Only the `workspace/executeCommand`
  routes wait on the re-open barrier (execute-command-routing-token, fail-soft
  if unsettled); other request paths open their own documents and do not
  wait. Restoration is therefore an observable catch-up window: `restart`
  resolves at verified `Ready` (the completion transition above), the re-open
  sweep runs after it, `documents` may briefly under-report, and a
  pass-through request racing the sweep is — like all pass-through — the
  caller's own risk.
- **A shared instance re-seeds; nothing is remembered.** A `#shared` key
  carries no root, and the old handle's accumulated folder set dies with
  it. Because no triggering document exists to resolve a marker root — the
  existing acquire path cannot revive a dead shared key without one — the
  replacement is seeded with a **single** root, the client's primary root:
  the same one-root shape a fresh spawn gets, and deliberately not the
  client's whole folder list, which the incapable-server capability
  fallback would misread as already-served. The set regrows through the
  existing add-only acquire path
  (`workspace/didChangeWorkspaceFolders` per
  ls-bridge-server-pool-coordination) as roots re-acquire the shared
  connection — derive, don't remember, applied to folders. One ordering
  obligation falls on the re-open sweep: it acquires the replacement by
  key, bypassing the ordinary acquire path that announces new shared
  roots, so for a shared replacement the sweep must add-and-announce each
  document's root before that root's first `didOpen` — otherwise non-seed
  documents reopen on a server that was never told about their folder. In a
  workspace-less session (initialize carried neither `rootUri` nor
  `workspaceFolders`), the replacement spawns rootless with an empty folder
  seed — the same shape no-workspace sessions already give fresh spawns —
  and folders join as marker roots acquire it. If the
  replacement no longer advertises workspace-folder change support,
  pool-coordination's existing capability fallback applies: subsequent
  acquires degrade to per-root connections and the restarted shared slot
  simply serves nothing new. One piece of routing metadata **is**
  retained across the stopped, termination-pending, and
  in-flight-operation records:
  the shared slot's workspace-folder **capability verdict**, which that
  fallback consults. The live decision reads it from the `Ready` handle,
  and the handle dies with a stop — without the retained verdict,
  non-seed roots of an *incapable* shared server would resolve
  optimistically to the shared key while it is stopped and hit its
  fence, blacking out per-root clients other roots already use. With
  it, their acquires keep resolving to per-root keys exactly as when
  the handle was live.

During the restart window, `request` fails with `clientRestarting` and
`notify` is silently dropped. `restart` on a `stopped` slot is simply a
start. `restart` on a `failed` slot replaces the process the way a later
acquire would — `Failed` connections respawn on acquire, not on a timer —
and additionally **clears the slot's consecutive-panic count**: the pool
disables a slot after `MAX_CONSECUTIVE_PANICS` consecutive initialization
handshake panics (the counter resets on a successful handshake), and an
explicit user-initiated `restart` is exactly the signal that should re-arm
it. For the same reason, `restart` is exempt from any future crash-storm
rate limiter (ls-bridge-server-pool-coordination Phase 2): a human asking is
not a storm.

## Considered Options

### Address clients by server name instead of ids

Rejected: per-root pooling means one name maps to N connections (one per
marker root, plus fallback and shared variants). Every control method would
need its own disambiguation parameters, re-inventing the key.

### Per-process opaque ids (fresh UUID each spawn)

A generation id would distinguish "the process before restart" from "after".
Rejected: the protocol's contracts (stable handle across `restart`,
`restart → null`, stopped slots addressable for revival) all want *slot*
semantics; a generation id would invalidate every held handle on each restart
and force every client to carry a re-enumeration loop. It would also need a new
id→connection registry, where `ConnectionKey` already keys everything.

### Embed `rootUri` in the enumeration result instead of a `workspaceFolders` request

Rejected: a shared-instance connection serves a growing *set* of folders; a
scalar field misrepresents it. The `Display` id already gives humans the
root at a glance; programs that need the folder set ask the dedicated
request.

### Return the downstream `ResponseMessage` verbatim

Rejected: `jsonrpc` is noise and `id` is actively misleading (it is the
bridge's internal downstream request id, not the caller's). `{ result?,
error? }` keeps exactly the information the caller can use.

### Auto-respawn a stopped client on the next matching `didOpen`

Rejected: it turns `stop` into advice. A user who stopped a misbehaving server
would have it resurrect on the next keystroke in a matching document — a
hidden resurrection with no observable trigger. Explicit `restart` keeps the
lifecycle in the caller's hands; the cost (features silently dark on matching
documents while stopped) is surfaced by `status: "stopped"` in the
enumeration.

### Re-key `restart` under current rooting configuration

If `preferSharedInstance` has been enabled since the spawn, "restart" could
tear down `tsgo@file:///repo/a` and bring up `tsgo#shared`. Rejected: the
held id would dangle, so `restart` would have to return a new id (breaking
the stable handle contract), and every held copy of the old id would
silently rot.
Spawning the replacement under the identical key is simpler and honest:
rooting changes apply to newly routed documents. A restarted slot under
changed rooting re-opens nothing — derivation runs against current settings —
so it comes up empty and sits idle rather than serving stale routes.

### Bridge-defined error codes instead of `data.reason`

A custom code per failure (`-32001` stopped, `-32002` restarting, …) is
machine-readable without parsing `data`. Rejected: `RequestFailed` is the
LSP-blessed code for exactly this class — per the spec, "a request failed but
it was syntactically correct, e.g the method name was known and the
parameters were valid" — the bridge already uses it for its other failures,
and a `data` discriminator extends without burning through a scarce code
namespace.

## Consequences

### Positive

- The connection pool becomes observable and controllable from the editor
  side: a wedged downstream can be inspected and restarted without restarting
  kakehashi.
- Pass-through opens downstream capabilities the bridge does not yet
  translate, making the bridge a platform rather than a bottleneck
  (mirroring node-reference-protocol's goal for the syntax tree).
- Nearly every mechanism is reuse: `ConnectionKey` routing,
  `forward_cancel_downstream`, the graceful-shutdown state machine, the
  ARM/CLAIM derived re-open. The genuinely new state is the **lifecycle
  actor**, its pool supervisor, and what the actor owns (the stopped set,
  termination-pending records, per-key operation state, the
  per-connection shutdown timeout), plus the
  in-flight pass-through id map, the per-handle `serverInfo`, the
  liveness classification on router pending entries, and the deny list.
- Held ids survive restarts, so tooling built on the protocol needs no
  re-enumeration choreography.

### Negative

- Pass-through can desynchronize the *downstream's* state from the caller's
  expectations (documents opened outside the bridge's knowledge, requests
  about never-opened documents). The managed/self-responsibility boundary is
  documented, but misuse is not detectable by the bridge.
- A stopped slot silently disables bridge features for every matching
  document until restarted. This is the requested semantic, but it is a
  footgun for a user who forgets a `stop`; the enumeration's
  `status: "stopped"` is the only breadcrumb.
- Spawn commits route through the lifecycle actor — one message round trip
  per spawn decision, but a new coupling between the control protocol and
  the acquire path (existing `Ready` connections stay lock-free, so the
  steady-state hot path is untouched).
- The id is contractually opaque, yet its rendering is legible and users will
  inevitably parse it (Hyrum's law). Changing the `Display` format later will
  break such clients — documented as unsupported, not prevented.
- The deny list is a judgment call that must track protocol evolution: a
  future LSP lifecycle method would corrupt bridge state until it is added
  to the list.

### Neutral

- Custom methods under the `kakehashi/` namespace, with a
  kakehashi-native domain (`bridge/client`) in the scope slot — the precedent
  set by `kakehashi/node*` and `kakehashi/captures/*`, which also name
  kakehashi-defined domains rather than LSP scopes. A pending convention
  (branch `feat/scope-first-custom-methods`) mandates LSP scopes
  (`textDocument/`, `workspace/`) for methods that shadow LSP features;
  whichever lands second must reconcile explicitly — these methods shadow no
  LSP feature, so the intended reading is that the LSP-scope rule does not
  apply to them, and this ADR records that intent.
- The `capabilities.experimental` announcement is a new discovery
  convention, not an existing one; the older `kakehashi/*` families remain
  undiscoverable until someone backfills them.
- Server-name validation (`@`/`#` rejected) constrains configuration slightly;
  no known real-world server name uses either character.

## Implementation Notes

- Methods register via `LspService::build().custom_method(...)` following the
  `kakehashi/node*` pattern; handlers live under
  `src/lsp/lsp_impl/kakehashi/bridge/client/`.
- Id resolution: render each live pool key, termination-pending-record
  key, stopped-set key, and in-flight-operation key with `ConnectionKey`'s
  `Display` and compare for exact equality with the supplied id; no
  parsing. The last three all live in the lifecycle actor's state, read
  through a snapshot it publishes atomically per transition. Owner
  precedence for enumeration is live handle > termination-pending record
  (`stopping`) > stopped set > in-flight operation, deduplicated to one
  row — a `Closed` handle awaiting removal is ignored and falls through
  that same order (`Closed` deliberately has no `Client.status` of its
  own). The in-flight-operation entry matters during the restart window,
  when the stopped entry is already cleared and no handle exists yet: it
  is the key's only owner then, so without it the slot would vanish from
  enumeration and calls would answer `unknownClient` instead of
  `clientRestarting`. An operation-only key enumerates by its phase —
  `stopping` for an in-flight `stop` and for a restart still in its stop
  phase (a reload can purge the `Closing` handle, leaving the operation
  as sole owner in either case), `starting` once a restart's respawn has
  begun — and an ordinary acquire's pre-handle `Spawning` intent entry
  is an in-flight-operation record like any other: it enumerates as
  `starting`, resolves its id, and fences acquires until the handle
  lands or the intent settles. A `Spawning` entry retained in its
  **settling** phase (claimed by `stop`, `restart`, or teardown)
  enumerates as `stopping` — it is being wound down — with one
  exception: reload deletion follows the reload rule, dropping the
  entry from the published snapshot so the id resolves `unknownClient`
  while the fenced cleanup record persists internally, unaddressable.
  Visibility follows configuration: a later reload that re-adds the
  same server name makes the surviving record addressable again — it
  enumerates as `stopping` and still fences acquires until cleanup
  completes. What happens then follows the record's **origin**, which
  survives reload deletion and re-addition: stop-origin cleanup installs
  the tombstone (the user's stop is still in force); restart-origin cleanup installs the
  config-revalidated **retry** tombstone the ownership-at-completion
  rule requires, so a later `restart` retries; reload-origin cleanup
  simply **dissolves** — no user ever stopped the slot, so it returns to
  ordinary acquire-driven spawning. `ConnectionState` alone could never provide this: it exposes
  only `Initializing`, which cannot distinguish a restart in flight from
  an ordinary first spawn.
- Acquire keeps using existing `Ready` connections lock-free; the
  spawn-commit path becomes a lifecycle-actor message, where the stopped
  set, termination-pending records, in-flight operations, and teardown
  sealing are all checked in-queue.
- `restart` clears the slot's entry in `consecutive_panic_counts` before
  respawning; `stop` drives `force_kill_with_escalation` from a new
  per-connection timeout rather than the pool-wide teardown path.
- `serverInfo` needs new per-handle state: the handshake currently retains
  only `ServerCapabilities`, so the initialize result's `serverInfo` must be
  parsed and stored on the connection handle.
- Pass-through cancellation reuses `forward_cancel_downstream` keyed by
  `(ConnectionKey, downstream id)`. As in the formatting pipeline, the
  handler itself records the downstream id it minted for each in-flight
  pass-through — there is no registry to consult — and that
  outer-id → downstream-id map is part of the protocol's new state. The
  handler must consume the request tracker's latched cancellation before,
  or atomically with, enqueueing the inner request; recording the mapping
  only after the enqueue leaves a window where an already-latched cancel is
  lost and the unbounded request it was the only escape from survives.
- Excluding pass-through from Tier-2 liveness needs a per-entry
  classification on the response router's pending map, and the
  classification must govern the whole accounting lifecycle — the 0→1
  transition that arms the timer, epoch advancement, and the →0 stop — not
  just expiry filtering. Today every pending entry counts, so an
  unclassified slow pass-through would still fail the connection, and a
  raw-only entry could equally prevent a later managed request from arming
  liveness at all.
- The router must return a typed, provenance-bearing outcome: today
  downstream responses and locally synthesized failures travel the same
  raw-JSON channel, but the envelope contract needs to tell "the
  downstream's error, verbatim" apart from bridge-synthesized
  `connectionLost`/`forwardFailed`.
- Forwarding "verbatim" includes params omission: the existing typed
  message builders always serialize a `params` field, so pass-through
  needs a raw builder that preserves an absent inner `params` instead of
  normalizing it to `null` (a shape JSON-RPC does not permit and some
  servers reject).
- Server-name validation is the first key validation `languageServers` gets —
  none exists today. A name containing `@` or `#` is rejected with a
  user-facing notice and the entry is never spawned, matching the
  warn-and-continue posture of the existing config advisories.

## Summary

| Aspect | Decision |
|---|---|
| **Namespace** | `kakehashi/bridge/client`, `kakehashi/bridge/client/{request,notify,documents,serverInfo,serverCapabilities,workspaceFolders,stop,restart}` |
| **Client id** | `ConnectionKey` `Display` string; contractually opaque; slot-stable across restarts |
| **Name validation** | `languageServers` keys may not contain `@` or `#` |
| **Pass-through** | Verbatim, untranslated; deny `initialize`/`initialized`/`shutdown`/`exit`/`$/cancelRequest` |
| **Response envelope** | `ForwardResult`: exactly one of `result` (always emitted, may be `null`) or `error`; framing fields stripped |
| **Errors** | `RequestFailed` (`-32803`) + `data.reason` discriminator; fail fast, never queue |
| **Cancellation** | Outer `$/cancelRequest` forwarded to the inner downstream request; outer fails `RequestCancelled`; no bridge-imposed timeout, no Tier-2 liveness accounting |
| **`stop`** | Graceful handshake when `running` (init-abort when `starting`, handshake bypass when `failed`), bounded by a new per-connection timeout, then forced escalation; stopped set pins the slot until explicit `restart`; single-flight per key |
| **`restart`** | Same key, current process-level config, no re-key; derived re-open (ARM/CLAIM); resolves only after the `Ready` replacement is verified pool-resident, else `restartFailed`; clears the panic count |
| **Lifecycle control** | Every lifecycle transition serializes through the pool's lifecycle actor (ls-bridge-graceful-shutdown § Lifecycle Actor); single-flight, acquire fencing, and abort-safety are consequences of the queue |
| **Discovery** | Announced as `capabilities.experimental.kakehashi.bridgeClient: true` |
