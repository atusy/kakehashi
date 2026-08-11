# Bridge Client Control Protocol

**Related Decisions**:
- [node-reference-protocol](node-reference-protocol.md) — precedent for a handle-based `kakehashi/*` custom-method family
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the `ConnectionKey` identity this protocol exposes
- [ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md) — the shutdown handshake `stop` reuses
- [respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md) — the derived re-open `restart` relies on
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — cancellation forwarding the pass-through request reuses

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
| `kakehashi/bridge/client/request` | request | `{ id, method, params? }` | `{ result?, error? }` |
| `kakehashi/bridge/client/notify` | notification | `{ id, method, params? }` | — |
| `kakehashi/bridge/client/documents` | request | `{ id, documentSelectors? }` | `OpenDocument[]` |
| `kakehashi/bridge/client/serverInfo` | request | `{ id }` | `ServerInfo \| null` |
| `kakehashi/bridge/client/workspaceFolders` | request | `{ id }` | `WorkspaceFolder[] \| null` |
| `kakehashi/bridge/client/stop` | request | `{ id }` | `null` |
| `kakehashi/bridge/client/restart` | request | `{ id }` | `null` |

The methods are announced under the initialize result's
`capabilities.experimental` so clients can feature-detect before use.

### Client Identity: the `ConnectionKey` Display String

```typescript
type Client = {
  name: string;    // config server name (the `language_servers` map key)
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
   connections, document versions, the cancel registry, the respawn ARM/CLAIM
   set — is keyed by `ConnectionKey`, so id-based routing needs no new
   registry.
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
Configuration loading therefore rejects `language_servers` names containing
`@` or `#`.

**`status`** mirrors the connection state machine
(ls-bridge-graceful-shutdown): `starting` = Initializing, `running` = Ready,
`stopping` = Closing, `failed` = Failed, and `stopped` = explicitly stopped via
`stop` (see below). Stopped slots **are included** in the enumeration — they
are absent from the live pool, and the enumeration is the only way to recover
their id for a later `restart`.

### Enumeration: `kakehashi/bridge/client`

Both parameters are optional and compose as AND:

- `textDocument?: TextDocumentIdentifier` — only clients that serve this
  document: the document (or one of its injections) bridges to the server
  *and* resolves to this connection's root.
- `name?: string` — only clients spawned from this `language_servers` entry.

With neither parameter, all clients (including stopped slots) are returned.

### Pass-Through: `request` and `notify`

`params: { id, method, params? }` — the inner `method`/`params` are forwarded
to the downstream connection **verbatim**: no URI rewriting, no position
mapping, no capability filtering, no document-lifecycle bookkeeping. This is
the deliberate boundary between the bridge's managed domain and the caller's
self-responsibility domain:

- The bridge guarantees its *own* state stays coherent (its didOpen/didChange
  sync, its virtual documents, its capability records are untouched by
  pass-through traffic).
- The bridge does **not** guarantee the downstream's view stays coherent with
  the caller's expectations. A pass-through `textDocument/didOpen` creates a
  document the bridge does not know about; a request naming a document the
  bridge never opened is forwarded anyway (this holds after `restart` too — the
  derived re-open restores only bridge-managed documents).

**Denied methods.** Five methods would not extend the downstream's state but
corrupt the bridge's own protocol state machine, and are rejected:
`initialize`, `initialized`, `shutdown`, `exit`, and `$/cancelRequest`
(cancellation is expressed by cancelling the outer request — below). A denied
`request` fails with `data.reason: "methodDenied"`; a denied `notify` is
dropped and logged (notifications have no response channel).

**Response envelope.** The outer request succeeds whenever forwarding
succeeded, and its result wraps the downstream's answer:

```typescript
type ForwardResult = {
  result?: LSPAny;         // exactly one of the two is present
  error?: ResponseError;   // the downstream's error, verbatim
};
```

The JSON-RPC framing fields (`jsonrpc`, `id`) are stripped: the downstream
`id` is a bridge-internal request id, unrelated to the caller's, and echoing
it would only invite confusion. Outer-level errors (unknown id, stopped
client, denied method) use the error model below, so callers can distinguish
"the bridge could not forward" from "the server answered with an error".

**Cancellation.** A `$/cancelRequest` for the *outer* request id is forwarded
to the downstream as a cancel of the inner request, via the existing cancel
registry (ls-bridge-message-ordering § Cancellation Forwarding). Without this,
a hung downstream would pin the outer request forever.

### Error Model

Bridge-level failures use `RequestFailed` (`-32803`) with a machine-readable
discriminator in `data`:

```jsonc
{ "code": -32803, "message": "bridge/client: <human summary>",
  "data": { "reason": "unknownClient" } }
```

| `data.reason` | Meaning |
|---|---|
| `unknownClient` | id matches no current slot |
| `clientStopped` | slot explicitly stopped; `restart` revives it |
| `clientRestarting` | slot is mid-`restart`; retry after it returns |
| `methodDenied` | inner method is on the deny list |

`notify` never errors: during `stopping`/`stopped`/`restarting` it is silently
dropped (logged at debug level), matching JSON-RPC notification semantics.

### Inspection: `documents`, `serverInfo`, `workspaceFolders`

```typescript
type OpenDocument = {
  uri: string;         // as the downstream sees it (virtual URI for injections)
  languageId: string;
  version: number;     // last version sent downstream
  hostUri?: string;    // host document a bridge-minted virtual document derives from
};
```

- `documents` returns the bridge-managed open documents of the connection —
  virtual documents included, distinguished by `hostUri`. Documents opened via
  pass-through are invisible here (outside the managed domain, by the boundary
  above). `documentSelectors?: DocumentSelector[]` filters with standard LSP
  DocumentSelector semantics against the downstream-facing `uri`/`languageId`;
  omitted or `null` returns all.
- `serverInfo` returns the `serverInfo` field of the downstream's initialize
  result, which is optional in LSP — hence nullable.
- `workspaceFolders` returns the folder set the bridge maintains for the
  connection (`WorkspaceFolderSet` for shared instances, the single root
  otherwise), as `WorkspaceFolder[] | null`. A dedicated request — rather than
  a `rootUri` field on `Client` — because a `preferSharedInstance` connection
  serves *many* folders and a scalar field cannot represent that.

### `stop`: Graceful, and Pinned Until Explicit Restart

`stop` runs the existing graceful shutdown (ls-bridge-graceful-shutdown):
`Closing` → LSP `shutdown`/`exit` handshake → `Closed`, with the forced
SIGTERM/SIGKILL escalation on timeout. In-flight and newly arriving operations
fail per that decision's disposal policy. The result `null` is returned when
the slot reaches `Closed`.

What is new is the **stopped set**: the pool records the `ConnectionKey` as
explicitly stopped, and the normal routing path consults it — a `didOpen` (or
any acquire) that resolves to a stopped key does **not** spawn. The slot's
features stay dark until a `kakehashi/bridge/client/restart` clears the entry.
This is a deliberate behavior change to the normal path: without it, the next
keystroke in a matching document would resurrect the server and `stop` would
be advisory. `stop` on an already-stopped slot returns `null` (idempotent).

### `restart`: Same Key, Current Config, Derived Re-Open

`restart` = graceful stop (if running) + clear the stopped entry + respawn the
**same** `ConnectionKey` under the configuration current at that moment, then
return `null` once the replacement reaches Ready.

- **Process-level configuration applies.** `command`, `args`,
  `initializationOptions`, settings — whatever the config says *now* is what
  the replacement is spawned with.
- **No re-key.** Key-*defining* configuration (`root_markers`,
  `preferSharedInstance`) does not re-route the existing slot: the replacement
  is spawned under the identical key, and rooting changes take effect only for
  newly routed documents. This is what keeps held ids stable; the alternative
  is rejected below.
- **Document restoration is derived, not remembered.** The respawn goes
  through the existing purge→ARM, handshake→CLAIM mechanism, so the
  replacement re-opens whichever currently open documents belong to it, per
  respawn-reopen-derives-its-targets. First requests after `restart` may wait
  on that decision's barrier.

During the restart window, `request` fails with `clientRestarting` and
`notify` is silently dropped. `restart` on a `stopped` slot is simply a start;
`restart` on a `failed` slot replaces the process like any respawn.

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
rooting changes apply to newly routed documents, and the old slot drains
naturally as documents close.

### Bridge-defined error codes instead of `data.reason`

A custom code per failure (`-32001` stopped, `-32002` restarting, …) is
machine-readable without parsing `data`. Rejected: `RequestFailed` is the
LSP-blessed code for exactly this class ("the request failed for reasons that
are not the client's fault"), the bridge already uses it for its other
failures, and a `data` discriminator extends without burning through a scarce
code namespace.

## Consequences

### Positive

- The connection pool becomes observable and controllable from the editor
  side: a wedged downstream can be inspected and restarted without restarting
  kakehashi.
- Pass-through opens downstream capabilities the bridge does not yet
  translate, making the bridge a platform rather than a bottleneck
  (mirroring node-reference-protocol's goal for the syntax tree).
- Nearly every mechanism is reuse: `ConnectionKey` routing, the cancel
  registry, the graceful-shutdown state machine, the ARM/CLAIM derived
  re-open. The genuinely new state is one stopped set and the deny list.
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

- Custom methods under the `kakehashi/` namespace, announced via
  `capabilities.experimental` — consistent with the existing families.
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
- Pass-through cancellation reuses `forward_cancel_downstream` keyed by
  `(ConnectionKey, downstream id)`, as the formatting pipeline already does.
- Server-name validation lands in configuration loading with the other
  `language_servers` key checks.

## Summary

| Aspect | Decision |
|---|---|
| **Namespace** | `kakehashi/bridge/client`, `kakehashi/bridge/client/{request,notify,documents,serverInfo,workspaceFolders,stop,restart}` |
| **Client id** | `ConnectionKey` Display string; contractually opaque; slot-stable across restarts |
| **Name validation** | `language_servers` keys may not contain `@` or `#` |
| **Pass-through** | Verbatim, untranslated; deny `initialize`/`initialized`/`shutdown`/`exit`/`$/cancelRequest` |
| **Response envelope** | `{ result?, error? }` — framing fields stripped |
| **Errors** | `RequestFailed` (`-32803`) + `data.reason` discriminator |
| **Cancellation** | Outer `$/cancelRequest` forwarded to the inner downstream request |
| **`stop`** | Graceful handshake; stopped set pins the slot until explicit `restart` |
| **`restart`** | Same key, current process-level config, no re-key; derived re-open (ARM/CLAIM) |
| **Discovery** | Announced under `capabilities.experimental` |
