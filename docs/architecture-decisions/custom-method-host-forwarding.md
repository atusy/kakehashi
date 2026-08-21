# Custom Method Host Forwarding

**Related Decisions**:
- [host-document-bridge](host-document-bridge.md) — the `_self` layer whose `enabled` gate, server selection, and aggregation map this decision reuses verbatim
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — the ordered-allowlist `priorities` semantics that pick the receiving servers
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — the `preferred` strategy, the only one a method of unknown shape can use
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — cancel forwarding and the upstream-id registry the forwarded request joins
- [cross-layer-aggregation](cross-layer-aggregation.md) — the virt/host/native walk that forwarded methods deliberately do **not** join (host only)

## Implementation Status

Implemented. The fallback dispatch lives in `src/lsp/custom_method_forwarder.rs`
(`CustomMethodForwarder`), the handlers in
`src/lsp/lsp_impl/custom_method_forward.rs`, and the gate-free host sends in
`src/lsp/bridge/text_document/host.rs`.

## Context

Kakehashi implements a fixed set of LSP methods. A client that speaks a method
outside that set — `textDocument/inlineCompletion` to a Copilot server, a
vendor-specific `$/…`-free custom request, or a custom notification — gets
`MethodNotFound` from the JSON-RPC router and the downstream server never
hears about it, even when the user has a server configured that answers it.

Adding one typed handler per such method does not scale: the parameter and
result shapes are unknown to kakehashi, and the set is open-ended. What *is*
known for every JSON-RPC message is whether it is a request (has `id`) or a
notification, and — for the textDocument family — which host document it
concerns (`params.textDocument.uri`).

The host layer (host-document-bridge) already forwards the client's params
verbatim with the real URI and returns the downstream `result` untranslated.
That path is shape-agnostic by construction; it only lacked a way in for
methods kakehashi has no handler for.

## Decision

A method kakehashi does not implement is forwarded to the host document's
servers when, and only when, the user has written an **explicit** aggregation
entry for it under `bridge._self`:

```toml
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."textDocument/inlineCompletion"]
priorities = ["copilot"]
# strategy defaults to "preferred"; "concatenated" is rejected for forwarded
# methods (the result shape is unknown, so nothing can be merged).
```

### Contract

- **Eligibility.** Three conditions, all required: kakehashi has no handler
  for the method; `params.textDocument.uri` names an open host document whose
  language has `bridge._self.enabled = true`; and the method name appears as
  a **literal key** of that language's `bridge._self.aggregation` map
  (inherited through the `bridge._` wildcard like any other aggregation
  field — which means the shipped defaults and `languages._` contribute
  keys too; today those name only router-handled methods). The `"_"`
  method wildcard does not make a method eligible — it would turn every
  typo into a downstream round trip. An ineligible request keeps today's
  `MethodNotFound`; an ineligible notification is not forwarded — silently
  when no configuration names the method at all, otherwise logged at debug
  (a malformed or misconfigured one at warn).
- **Eligible but empty.** An eligible entry with `priorities = []` or
  `maxFanOut = 0` selects no server: a request answers `null`, a
  notification goes nowhere — the per-method kill switch
  (aggregation-priorities-wildcard) behaves as it does for typed methods.
- **Routing key, not a restriction.** `params.textDocument.uri` is read to
  pick the host document; the params are otherwise not inspected, so a
  client can route any params shape by including that member.
- **Request vs. notification follows the wire.** A message with an `id` is
  forwarded as a request and answered with the first non-empty downstream
  `result` in `priorities` order (`preferred`, with the host layer's
  emptiness rule: `null`, `[]`, and the known empty-list envelopes such as
  `{"items": []}` count as empty — a bare `{}` does not, it is a result);
  an all-empty or all-failed fan-out answers `null`. A response carrying
  neither `result` nor `error` is a failed contribution on this path. A message without an `id` is forwarded as a
  notification to **every** selected server, each delivered independently.
- **Ordering.** Forwarded requests are not sequenced behind pending
  `didChange` tickets (the same as hover and definition) and sync the text
  snapshotted at dispatch, as typed host methods do. A forwarded
  notification, which may wait through a server's initialization first,
  re-reads the document's current text at sync time, and is dropped if the
  document was closed while it waited. That wait is not the client's:
  the notification's ingress handling completes once delivery is handed
  off, so a burst of forwarded notifications against a slow-starting
  server cannot stall the other messages behind it. Deliveries in flight
  are bounded; past the bound a notification is dropped with a warning
  rather than queued — waiting for a slot would move the stall back into
  the handler. No
  order is promised between forwarded notifications, even to one server:
  their handlers already run concurrently, as every non-sync message's do.
- **Startup.** A forwarded request follows the host-request policy and
  fails fast (empty) on a server still initializing — the next request gets
  it. A forwarded notification instead waits through initialization, within
  the initialization bound (ls-bridge-timeout-hierarchy Tier 0): a request
  is naturally retried, a dropped notification is gone.
- **Verbatim params, verbatim result.** Host-layer rules apply unchanged:
  real URI, real coordinates, no translation either way, progress tokens
  stripped (the bridge does not relay downstream progress).
- **Host only.** Injection regions are not consulted. A custom method's
  params cannot be re-targeted at a virtual document without knowing which
  fields are positions, so the virt layer is out of scope (see Consequences).
- **Capabilities are not consulted.** Unlike the typed host methods, the
  forward does not require the server to advertise anything. A server that
  does not implement the method is expected to answer `MethodNotFound`
  itself; that answer is an empty contribution (logged at debug, not a
  counted failure — it is the contract's normal decline, and a counted
  failure would warn the client on every keystroke). Any other downstream
  error is a counted failure, surfaced once per request as a client
  warning when no server answered. Kakehashi likewise advertises nothing to
  the client for forwarded methods; a client that gates on
  `ServerCapabilities` must be told to send anyway.
- **Strategy.** `preferred` only, judged on the entry's **own** `strategy`
  field: a `concatenated` inherited from the `"_"` method wildcard is
  ignored, as the typed verbatim host methods ignore it, so one wildcard
  line written for diagnostics cannot break every forwarded method. An
  entry that itself sets `strategy = "concatenated"` is a configuration
  error: the request is answered with `RequestFailed` (-32803; the request
  was well-formed, the server cannot serve it) naming the method, and a
  notification is dropped with a warning.
- **Reserved methods.** The bridge-owned lifecycle, sync, and routing
  methods (`initialize`, `shutdown`, `exit`, the `textDocument/did*` and
  `notebookDocument/*` sync families, `workspace/executeCommand`, the other
  notifications kakehashi handles itself) and the `$/` and `kakehashi/`
  namespaces are never forwarded, even when an entry names them: a request
  is answered `RequestFailed`, a notification is dropped with a warning.
  The forwarding methods can be called directly, so this holds in the
  handler, not only in the fallback dispatch. A direct call may still name
  a typed method kakehashi implements (say `textDocument/hover` with an
  entry written to tune its priorities); it is then sent blind to the host
  layer alone — the caller asked for exactly that, and the set of typed
  methods is what this decision refuses to enumerate.
- **Cancellation.** A forwarded request joins the upstream-id registry, so a
  client `$/cancelRequest` reaches the downstream servers exactly as it does
  for typed host methods.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- Forwarding never shadows a handler kakehashi has. For requests the
  router's own answer is the authority on "no handler", so no list of
  built-in request names exists to drift; notifications get no such answer,
  so the ones kakehashi handles are named exactly once, beside their
  implementation. An unimplemented standard notification is as forwardable
  as a custom one.
- A forwarded message reaches a server only after that server has the host
  document open at the text kakehashi considers current, under the same
  ordering guarantees as every typed host request; otherwise the server
  answers about a document it never opened, or about rolled-back text.
- A forwarded request is cancellable by the client's `$/cancelRequest`
  through the same path as a typed host request; no second bookkeeping.
- The misconfiguration surface is the request: `concatenated` or a reserved
  method fails the request that hits it rather than a settings-apply
  warning, because the set of built-in methods the warning would have to
  exclude is exactly what this decision refuses to enumerate.

## Consequences

### Positive

- Any request/notification a host-capable server understands becomes usable
  through kakehashi with two lines of config and no release.
- No per-method code: the existing host layer carries the whole feature.
- Built-in methods are unaffected by construction (router-first dispatch).

### Negative

- Host only. A Copilot-style server configured for an injection language
  cannot be reached this way; doing so needs a declared position schema per
  method (a `positions = ["position"]`-style annotation with response-side
  inverse mapping), deferred until a concrete need appears.
- No capability advertisement. Clients that consult `ServerCapabilities`
  before sending (most editors do for standard methods such as
  `inlineCompletion`) need a client-side override; a future `capability`
  field on the aggregation entry, merged into `ServerCapabilities` as raw
  JSON, would close this without guessing the method→capability mapping.
- `concatenated` is unavailable; merging needs a known result shape.
- A request whose params carry no `textDocument.uri` cannot be routed and is
  answered `InvalidParams` — the forward is intentionally limited to the
  document-scoped family rather than inventing a default target.
- When every selected server fails for a reason other than declining the
  method, the client sees `null` plus one `window/logMessage` warning per
  request, not the downstream error.

### Neutral

- The forward is exposed as the explicit methods `kakehashi/forward/request`
  and `kakehashi/forward/notification` with params `{ "method", "params" }`;
  the fallback dispatch rewrites an unhandled message into these. A client
  may call them directly; eligibility is checked identically either way.

## Alternatives Considered

### A. Per-method typed handlers (`inline_completion` etc.)

Rejected: every new method is a release, and the shapes kakehashi would have
to model are exactly the ones it has no use for.

### B. A hand-maintained list of built-in request names in the dispatch

Rejected: the list would drift from the router the moment a handler is added
or removed, and the failure mode (a built-in silently forwarded, or a custom
method silently dropped) is invisible until a user hits it. Asking the router
and acting on its `MethodNotFound` answer costs one extra in-process call only
for unhandled methods.

### C. Forward whenever `_self.enabled = true`, no per-method entry

Rejected: a wildcard forward turns every client typo and every unsupported
standard method into a downstream round trip, and makes the `"_"` aggregation
wildcard (which legitimately sets `priorities` for all typed methods) an
accidental opt-in.

### D. Virt-layer support via heuristic position translation

Rejected for now: guessing which fields are positions from their names works
for the request (`position`, `range`) but not for the response, whose shape is
unknown; an untranslated `InlineCompletionItem.range` would corrupt the
client's buffer. A declared schema is the right shape for that feature and is
deferred.
