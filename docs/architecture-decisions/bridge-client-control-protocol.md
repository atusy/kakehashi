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
| `kakehashi/bridge/client/workspaceFolders` | request | `{ id }` | `WorkspaceFolder[]` |
| `kakehashi/bridge/client/stop` | request | `{ id }` | `null` |
| `kakehashi/bridge/client/restart` | request | `{ id }` | `null` |

The methods are announced in the initialize result as
`capabilities.experimental.kakehashi.bridgeClient: true`, beside the existing
`wrappedDidChangeConfigurationSettings` flag, so clients can feature-detect
before use. This sets a new precedent rather than following one: the existing
`kakehashi/node*` and `kakehashi/captures/*` families are announced nowhere.

### Client Identity: the `ConnectionKey` Display String

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
(ls-bridge-graceful-shutdown): `starting` = `Initializing`, `running` =
`Ready`, `stopping` = `Closing`, `failed` = `Failed`, and `stopped` =
explicitly stopped via `stop` (see below). Stopped slots **are included** in
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
notification-kind method will never receive a downstream response and
resolves only through cancellation; a `notify` carrying a request-kind
method is a well-formed JSON-RPC notification the server will most likely
ignore. Both are the caller's responsibility. The inner `params` must be an
object, an array, or absent — the only shapes JSON-RPC permits — and
anything else is rejected at the outer level with `InvalidParams`
(`-32602`) before anything is forwarded.

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
envelope can never masquerade as a null result.

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
`RequestCancelled` (`-32800`); a downstream answer arriving after the cancel
was issued is dropped and logged. Without forwarding, a hung downstream
would pin the outer request forever — which matters because the outer
request carries **no bridge-imposed timeout**: the bridge cannot know the
expected latency of an arbitrary method, so the caller decides how long a
pass-through may take, and cancellation is the only way out. For the same
reason pass-through requests are excluded from Tier-2 liveness accounting
(ls-bridge-timeout-hierarchy): a deliberately slow custom request is not
evidence of a hung server and must not drive a healthy connection to
`Failed`. `notify`, having no response, is not cancellable. If the
connection dies after forwarding (crash, disposal), the outer request fails
with `data.reason: "connectionLost"` rather than fabricating a downstream
error.

**Pass-through is caller→downstream only.** Server-initiated traffic that a
pass-through call provokes — `$/progress`, `workspace/configuration`,
`workspace/applyEdit`, any server→client request — flows through the
bridge's normal inbound handling under bridge policy, exactly as if a
bridged request had provoked it. In particular, a caller-minted
`workDoneToken` riding the inner params is unknown to the bridge's progress
aggregation, so its progress is dropped; callers must not rely on progress
or server-request round trips through the escape hatch.

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
| `clientStopped` | slot explicitly stopped; `restart` revives it |
| `clientRestarting` | slot is mid-`restart`; retry after it returns |
| `restartFailed` | the `restart` replacement reached `Failed` instead of `Ready` |
| `connectionLost` | the connection died after the inner request was forwarded |
| `methodDenied` | inner method is on the deny list |

The bridge never queues or waits on behalf of a control call: a slot that is
not `running` fails fast with the reasons above.

`notify` never errors. It is forwarded while the slot is `starting` or
`running` (the order queue already accepts notifications during
initialization) and silently dropped — logged at debug level — during
`stopping`/`stopped`/`failed`, the restart window, and for unknown ids,
matching JSON-RPC notification semantics.

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
  boundary above). `documentSelector?: DocumentSelector` filters with
  standard LSP `DocumentSelector` semantics against the downstream-facing
  `uri`/`languageId`; omitted or `null` returns all. A `stopped` slot holds
  nothing open, so it answers `[]`.
- `serverInfo` returns the `serverInfo` field of the downstream's initialize
  result. `null` means exactly one thing — the downstream omitted the
  optional field. A slot that is not `running` fails with
  `clientStopped`/`clientNotReady` instead, so the two cases never blur.
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
on Unix; immediate kill on Windows). In-flight and newly arriving operations
fail per that decision's disposal policy. The result `null` is returned when
the slot reaches `Closed`, whichever path got it there. `stop` on a
`starting` slot is legal (`Initializing → Closing` is an existing transition)
and likewise returns at `Closed`.

What is new is the **stopped set**: the pool records the `ConnectionKey` as
explicitly stopped, and the normal routing path consults it — a `didOpen` (or
any acquire) that resolves to a stopped key does **not** spawn. The slot's
features stay dark until a `kakehashi/bridge/client/restart` clears the entry.
This is a deliberate behavior change to the normal path: without it, the next
keystroke in a matching document would resurrect the server and `stop` would
be advisory. `stop` on an already-stopped slot returns `null` (idempotent).

Two lifecycle rules keep the set coherent:

- **Control calls are single-flight per key.** `stop` and `restart` both
  span a handshake and mutate the same entry, so the bridge serializes them:
  a control call arriving while another is in flight on the same key fails
  fast (`clientRestarting` during a restart; `clientNotReady` with
  `data.status: "stopping"` during a stop) instead of interleaving into a
  state where a slot is simultaneously live and stopped.
- **The set is process-lifetime and config-checked.** A configuration reload
  drops stopped entries whose server name no longer exists in
  `languageServers` — their ids then resolve as `unknownClient` — while
  entries whose server survives the reload stay stopped. Nothing is
  persisted across kakehashi restarts.

### `restart`: Same Key, Current Config, Derived Re-Open

`restart` = graceful stop (if running) + clear the stopped entry + respawn the
**same** `ConnectionKey` under the configuration current at that moment. The
outer request resolves when the replacement reaches `Ready` (result `null`)
or `Failed` (error `restartFailed`), bounded by the existing initialization
timeout (ls-bridge-timeout-hierarchy). On failure the stopped entry stays
cleared, so the ordinary crash-respawn path still applies to the slot.

- **Process-level configuration applies.** `command`, `args`,
  `initializationOptions`, settings — whatever the config says *now* is what
  the replacement is spawned with.
- **No re-key.** Key-*defining* configuration (`workspaceMarkers`,
  `preferSharedInstance`) does not re-route the existing slot: the replacement
  is spawned under the identical key, and rooting changes take effect only for
  newly routed documents. This is what keeps held ids stable; the alternative
  is rejected below.
- **Document restoration is derived, not remembered.** The respawn goes
  through the existing purge→ARM, handshake→CLAIM mechanism, so the
  replacement re-opens whichever currently open documents belong to it, per
  respawn-reopen-derives-its-targets. Only the `workspace/executeCommand`
  routes wait on the re-open barrier (execute-command-routing-token, fail-soft
  if unsettled); other request paths open their own documents and do not
  wait.

During the restart window, `request` fails with `clientRestarting` and
`notify` is silently dropped. `restart` on a `stopped` slot is simply a
start. `restart` on a `failed` slot replaces the process like a crash
respawn, and additionally **clears the slot's consecutive-panic count**: the
pool disables a slot after `MAX_CONSECUTIVE_PANICS` crash respawns, and an
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
and force a re-enumeration loop into every client. It would also need a new
id→connection registry, where `ConnectionKey` already keys everything.

### Embed `rootUri` in the enumeration result instead of a `workspaceFolders` request

Rejected: a shared-instance connection serves a growing *set* of folders; a
scalar field misrepresents it. The Display id already gives humans the root
at a glance; programs that need the folder set ask the dedicated request.

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

If `preferSharedInstance` was enabled since the spawn, "restart" could tear
down `tsgo@file:///repo/a` and bring up `tsgo#shared`. Rejected: the held id
would dangle, so `restart` would have to return a new id (breaking the stable
handle contract), and every other holder of the old id would silently rot.
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
  ARM/CLAIM derived re-open. The genuinely new state is the stopped set, the
  per-connection shutdown timeout, the in-flight pass-through id map, and the
  deny list.
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
- The stopped-set check rides the normal acquire path — one map lookup per
  spawn decision, but a new coupling between the control protocol and the hot
  path.
- The id is contractually opaque, yet its rendering is legible and users will
  inevitably parse it (Hyrum's law). Renaming the Display format later will
  break such clients — documented as unsupported, not prevented.
- The deny list is a judgment call that must track protocol evolution: a
  future LSP lifecycle method would corrupt bridge state until added.

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
- Id resolution: render each live pool key (and stopped-set key) with
  `ConnectionKey`'s `Display` and compare for exact equality with the supplied
  id; no parsing.
- The stopped set lives beside the pool's per-connection maps, keyed by
  `ConnectionKey`; the acquire path checks it before any spawn decision.
- `restart` clears the slot's entry in `consecutive_panic_counts` before
  respawning; `stop` drives `force_kill_with_escalation` from a new
  per-connection timeout rather than the pool-wide teardown path.
- Pass-through cancellation reuses `forward_cancel_downstream` keyed by
  `(ConnectionKey, downstream id)`. As in the formatting pipeline, the
  handler itself records the downstream id it minted for each in-flight
  pass-through — there is no registry to consult — and that
  outer-id → downstream-id map is part of the protocol's new state.
- Server-name validation is the first key validation `languageServers` gets —
  none exists today. A name containing `@` or `#` is rejected with a
  user-facing notice and the entry is never spawned, matching the
  warn-and-continue posture of the existing config advisories.

## Summary

| Aspect | Decision |
|---|---|
| **Namespace** | `kakehashi/bridge/client`, `kakehashi/bridge/client/{request,notify,documents,serverInfo,workspaceFolders,stop,restart}` |
| **Client id** | `ConnectionKey` Display string; contractually opaque; slot-stable across restarts |
| **Name validation** | `languageServers` keys may not contain `@` or `#` |
| **Pass-through** | Verbatim, untranslated; deny `initialize`/`initialized`/`shutdown`/`exit`/`$/cancelRequest` |
| **Response envelope** | `ForwardResult`: exactly one of `result` (always emitted, may be `null`) or `error`; framing fields stripped |
| **Errors** | `RequestFailed` (`-32803`) + `data.reason` discriminator; fail fast, never queue |
| **Cancellation** | Outer `$/cancelRequest` forwarded to the inner downstream request; outer fails `RequestCancelled`; no bridge-imposed timeout, no Tier-2 liveness accounting |
| **`stop`** | Graceful handshake bounded by a new per-connection timeout, then forced escalation; stopped set pins the slot until explicit `restart`; single-flight per key |
| **`restart`** | Same key, current process-level config, no re-key; derived re-open (ARM/CLAIM); resolves at `Ready` or `Failed` (`restartFailed`); clears the panic count |
| **Discovery** | Announced as `capabilities.experimental.kakehashi.bridgeClient: true` |
