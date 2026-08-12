# Bridge Peer Protocol

**Related Decisions**:
- [bridge-client-control-protocol](bridge-client-control-protocol.md) — the planned editor-facing connection API; peer requests reuse its pass-through boundary but are available only to downstream servers
- [bridge-routing-protocol](bridge-routing-protocol.md) — the existing kakehashi→downstream custom request and the per-side dispatch rule
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — downstream request IDs, cancellation, response routing, and the single-writer connection transport
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the per-root `ConnectionKey` slots exposed as peers
- [ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md) — the managed downstream-request deadline inherited by peer requests

## Context

A downstream language server can decide a feature from project state more
precisely than static kakehashi configuration can. A meta-server such as tsudoi
may implement `textDocument/formatting`, inspect the project, and then choose
denols for one TypeScript document and `oxfmt --lsp` for another. It cannot make
that choice through the editor: the relevant language servers are kakehashi's
private downstream connections, and forwarding through the editor would expose
virtual-document and connection details on the wrong protocol side.

The planned bridge-client-control-protocol solves the sibling editor→kakehashi→
downstream case. This decision provides only the reverse ingress:
downstream→kakehashi→another downstream. The caller and target are peers from
kakehashi's point of view, but the caller must never discover or request itself.

## Decision

Introduce two custom requests handled only on downstream connections:

| Method | Input | Output |
|---|---|---|
| `kakehashi/bridge/peer` | `{ name?: string }` | `Peer[]` |
| `kakehashi/bridge/peer/request` | `{ id, method, params? }` | `ForwardResult` |

kakehashi advertises the API to downstream servers as
`initialize.params.capabilities.experimental.kakehashi.bridgePeer: true`.
Neither method is registered on the editor-facing LSP service.

### Peer Discovery

```typescript
type Peer = {
  name: string;                    // languageServers map key
  id: string;                      // opaque ConnectionKey slot identity
  workspaceFolders: WorkspaceFolder[];
};
```

Discovery returns only connections currently in `running` (`Ready`) state,
sorted by `id`. `name` optionally filters by the exact configured server name.
The exact calling `ConnectionKey` is always excluded; a same-name connection at
a different root remains a peer. Initializing, failed, stopping, closed, and
configured-but-not-running slots are absent. Discovery never starts a process.

`id` is contractually opaque. It identifies the `(server name, root mode/root)`
slot injectively even when a server name contains `@` or `#`; callers obtain it
from discovery and echo it without parsing. `workspaceFolders` is the target
connection's current folder snapshot, with a workspace-less/null snapshot
represented as `[]`.

### Peer Request

The selected peer receives the inner `method` and optional `params` unchanged,
under a fresh kakehashi-owned JSON-RPC request ID. An omitted `params` remains
omitted. No URI translation, capability filtering, document opening, or
virtual/host coordinate mapping occurs: the caller is responsible for using the
target's downstream-facing document identities and protocol state correctly.

`params`, when present, must be an object or array as required by JSON-RPC.
`initialize`, `initialized`, `shutdown`, `exit`, and `$/cancelRequest` are denied
because they would take over the target connection's lifecycle. Cancellation is
instead expressed by cancelling the outer request; kakehashi cancels the inner
request and answers the caller with `RequestCancelled` (`-32800`). Peer requests
inherit the ordinary managed downstream-request deadline (currently 30 seconds)
and Tier-2 liveness accounting.

The successful outer result strips the internal JSON-RPC fields and contains
exactly one branch:

```typescript
type ForwardResult =
  | { result: LSPAny }             // present even when null
  | { error: ResponseError };      // validated downstream error object
```

Bridge-level failures use `RequestFailed` (`-32803`) and `data.reason`:

| Reason | Meaning |
|---|---|
| `unknownPeer` | The id is absent, names the caller, or is not currently running |
| `methodDenied` | The inner method controls the connection lifecycle |
| `forwardFailed` | The inner request could not be queued |
| `connectionLost` | The target connection ended before answering |
| `requestTimeout` | The managed downstream request deadline elapsed |
| `malformedResponse` | The target returned an invalid JSON-RPC response envelope |

Malformed outer parameters use `InvalidParams` (`-32602`). A valid downstream
error remains data in a successful outer result; it is not confused with a
failure of kakehashi to perform the forwarding.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- The calling connection must never be returned or resolved as its own peer;
  excluding all connections with the same server name is also wrong because
  per-root pooling creates legitimate same-name peers.
- A cancel arriving after the outer request is accepted must release the caller
  and target the exact inner request; it must not be lost between forwarding and
  becoming cancellable or fan out to unrelated requests.
- Internal downstream request IDs must never escape in `ForwardResult`; they
  share neither identity nor lifecycle with the caller's outer ID.
- Per-side dispatch must remain strict: downstream peer methods are not an
  editor control surface, and planned editor-facing `bridge/client/*` methods
  are not thereby made callable from downstream connections.

## Considered Options

### Route the request through the editor

Rejected. It requires editor-specific cooperation, exposes downstream
connection identities on the wrong side, and cannot reliably preserve the
virtual-document state that kakehashi owns.

### Address a peer only by server name

Rejected. Per-root pooling permits several live connections with one configured
name. Choosing one would be ambiguous precisely in the monorepos where dynamic
project-aware selection is most valuable.

### Return configured servers and start one on request

Rejected for this initial protocol. Starting a configured server needs a root,
workspace, settings, and lifecycle decision that discovery alone cannot supply.
Returning only live peers makes the request target unambiguous and keeps this
API from becoming a second routing/spawn policy.

## Consequences

### Positive

- Meta-servers can select an already-running formatter or other provider from
  project state without making the editor understand kakehashi internals.
- The API respects per-root connection identity and exposes enough workspace
  context for a caller to select the intended slot.
- Arbitrary downstream methods become usable without adding a dedicated bridge
  implementation for each one.

### Negative

- Pass-through requests can desynchronize target protocol state. For example,
  an inner `textDocument/didOpen` or `didClose` is invisible to kakehashi's
  document tracker; this is the caller's responsibility.
- A peer is discoverable only after something else has started it. The protocol
  cannot select a configured but dormant formatter.
- Slow arbitrary requests share the existing timeout and liveness policy; they
  can contribute to a target connection being classified as failed.

### Neutral

- Discovery is a snapshot. A peer may stop after enumeration; the subsequent
  request then fails rather than silently selecting a replacement.
- The API grants no authority beyond the configured processes already running,
  but one trusted downstream process can ask another to perform any non-denied
  request it supports.

## Decision–Implementation Gap

None.
