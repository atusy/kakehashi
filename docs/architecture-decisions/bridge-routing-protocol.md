# Bridge Routing Protocol

**Related Decisions**:
- [bridge-client-control-protocol](bridge-client-control-protocol.md) — the sibling `kakehashi/bridge/*` family (client→kakehashi); the `capabilities.experimental` discovery convention and the opacity contract this protocol reuses
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the `ConnectionKey` model, per-root pooling, shared instances, and the workspace-folder capability fallback that `workspaceFolders` overrides interact with
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — the ordered-allowlist `priorities` semantics reused for provider selection
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — the `preferred` strategy this protocol dispatches with
- [ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md) — the timeout tiers the routing round-trip lives under
- [respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md) — the derived re-open that re-consults routing after a restart
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — the inheritance resolution applied before the config projection goes on the wire
- [host-document-bridge](host-document-bridge.md) — the `_self` layer whose aggregation entry governs host-document routing decisions

## Context

Routing — which downstream servers receive a document's `didOpen` and with
which workspace root each connection is keyed — is decided entirely by static
configuration: `languages` membership, `workspaceMarkers` resolution, and the
`enabled` flag. Nothing can decide routing *per document* from project state:
a monorepo tool that knows which sub-project owns a file, a policy layer that
enables a server only for directories that opt in, or a meta-server that
computes workspace topology cannot express any of that through kakehashi's
config alone.

bridge-client-control-protocol opened the connection pool to the editor-side
client. This decision opens the *routing policy* in the other direction:
kakehashi asks a downstream server how to route. The precedent for the shape
is `workspace/configuration` — the party holding the state is asked at the
moment the decision is needed — except here kakehashi is the asking client
and a downstream server is the answering authority.

## Decision

Introduce a kakehashi-initiated custom request, `kakehashi/bridge/routing`,
sent to downstream servers that advertise support. The answer overrides
kakehashi's own routing decision for one document; every absence — no
provider, `null` answer, missing server entry, timeout, error — falls back
to kakehashi's existing behavior. The protocol is fail-open at every layer.

### The Request

```typescript
type RoutingParams = {
  // The document being routed, as the downstream would see it: host
  // documents carry the real URI; injection regions carry the virtual URI
  // and the injection's languageId.
  textDocument: { uri: string; languageId: string };
  // The candidate set: every configured server that is enabled and whose
  // `languages` match the document's languageId, projected to the
  // routing-relevant fields, with wildcard inheritance already resolved.
  languageServers: Record<string, {
    languages?: string[];
    workspaceMarkers?: (string | string[])[];
  }>;
};

type RoutingResult = null | {
  routing: Record<string, {
    // Workspace folder URIs for this server. Absent or null = kakehashi
    // resolves the root as usual; [] = a rootless (workspace-less)
    // connection; non-empty = root-resolution override (see below).
    workspaceFolders?: string[] | null;
    // false = this document's didOpen is not forwarded to this server.
    enabled: boolean;
  }>;
};
```

- `textDocument` is required. Virtual URIs are **contractually opaque** to
  providers, exactly as bridge-client-control-protocol's client ids are to
  editors: providers must treat the string as an identity token, never parse
  its rendering, which may change between kakehashi versions.
- `languageServers` is a **projection**, not the configuration. Only
  `languages` and `workspaceMarkers` are sent — the two fields a routing
  decision can use — after wildcard-config-inheritance resolution, so the
  provider sees effective values, not the sparse user file. `cmd`,
  `initializationOptions`, and `settings` are never sent: they carry no
  routing signal, `settings` is an opaque user value that may hold
  credentials, and shipping the raw config struct would freeze kakehashi's
  configuration schema into a wire contract. The projection is a
  compatibility surface of its own and evolves additively only.
- Servers whose effective `enabled` is `false` are excluded from the
  projection entirely, so a provider cannot re-enable what configuration
  disabled — the record it would need to address is absent.

### Answer Semantics

The result layers "kakehashi decides" defaults at every granularity:

- `null` result — kakehashi decides everything, exactly as without the
  protocol.
- A server missing from `routing` — kakehashi decides for that server.
- `enabled: false` — the document's `didOpen` is not forwarded to that
  server. This is **per-document forwarding suppression**, not slot control:
  the connection (if any) is untouched, other documents route normally, and
  because the document is never opened there, no later request for it
  reaches that server either. It is deliberately weaker than
  bridge-client-control-protocol's `stop`, which pins a whole slot.
- Unknown server names in `routing` are ignored with a warning. An entry
  whose `workspaceFolders` contains a string that does not parse as a URI is
  dropped whole with a warning (kakehashi decides for that server). A result
  that does not deserialize to `RoutingResult` at all is treated as `null`
  with a warning — fail-open, never fail-closed.

**Precedence** is fixed: bridge-client-control-protocol's **stopped set >
routing answer > configuration**. A routing answer never resurrects a
stopped slot — `stop` is not advice (that decision's own language), and a
downstream server must not be able to undo an explicit user pin.

### `workspaceFolders` and the `ConnectionKey` Model

A non-empty `workspaceFolders` overrides root resolution — the step where
`workspaceMarkers` would walk the document's ancestors — for this
(document, server) decision. The override maps onto
ls-bridge-server-pool-coordination's model as follows:

- **Shared instance** (`preferSharedInstance` honored, server capable): the
  folder set joins the shared connection's `WorkspaceFolderSet` through the
  existing add-only announce path (`workspace/didChangeWorkspaceFolders`),
  and the document opens on the shared connection.
- **Per-root connections** (by configuration, or via the capability
  fallback): the **first element is the primary root** — the document opens
  on `ConnectionKey (server, first)`. Remaining elements are *warmed*: a
  connection per element is ensured through the ordinary acquire path, but
  this document's `didOpen` goes only to the primary. A document opens on
  exactly one connection per server name — the invariant every aggregation
  and translation path assumes — and the warm connections serve future
  documents whose own routing answers name those roots first.
- `[]` produces the rootless connection shape that workspace-less sessions
  already use for fresh spawns.

Folder strings are workspace folder URIs; the folder `name` is derived the
same way an ordinary spawn derives it.

### Discovery

A downstream server advertises support in its initialize **result**:
`capabilities.experimental.kakehashi.bridgeRouting: true`. This mirrors the
convention bridge-client-control-protocol set on the client-facing side, and
like `serverInfo` there, the flag is parsed from the initialize result and
retained per connection handle. Kakehashi never sends the request to a
server that did not advertise — which is also what makes the default
configuration free: a fleet with no advertising server pays zero round
trips.

### Provider Selection: Aggregation Machinery, `preferred` Dispatch

Provider order is configured through the existing per-method aggregation
map — `languages.<host>.bridge.<lang>.aggregation`, abbreviated
`bridge.<lang>.aggregation` below as in its sibling ADRs — under the method
key `kakehashi/bridge/routing`:

```toml
[languages._.bridge._.aggregation."kakehashi/bridge/routing"]
priorities = ["policy-server", "*"]
```

An injection region's routing decision reads the injection language's
`bridge.<lang>` entry; a host document's decision reads the host-layer
`bridge._self` entry (host-document-bridge), each falling back through the
usual wildcards.

- `priorities` follows aggregation-priorities-wildcard unchanged: ordered
  allowlist, `"*"` for the unlisted rest, `None` inherits to `["*"]`, and an
  explicit `[]` disables routing queries for the language. No
  routing-specific default is invented — the advertisement gate above is
  what keeps `["*"]` safe.
- The candidate providers for a query are the servers that are bridge
  candidates for the document's language **and** advertise `bridgeRouting`
  **and** are running (or within the bounded initialization wait below).
  They are asked in `priorities` order; the first non-`null` answer wins.
  An error, timeout, or malformed answer counts as `null` and the next
  provider is asked.
- The method consumes only `priorities`. `strategy`, `maxFanOut`,
  `pullFallback`, and `pushFallback` are not consumed — the request
  dispatches `preferred` regardless, the same posture
  language-server-bridge-request-strategies records for every method that
  does not consume a strategy.
- Because the key space of `bridge.<lang>.aggregation` is method names, a
  per-language provider order (`bridge.python.aggregation.…`) works with no
  new machinery. This is the first non-LSP method in that key space.

Two structural rules keep the protocol from consuming itself:

- **No recursion.** Establishing a connection to a provider, and the routing
  query itself, never trigger a routing query. Provider connections are
  routed by kakehashi's own rules — the bootstrap base case.
- **Document-independent traffic.** The routing request rides the provider's
  connection like any managed request but needs no open document. A provider
  that answers `enabled: false` for itself therefore keeps working: its
  documents are suppressed, its policy channel is not.

The self-referential corner composes into a useful pattern: a dedicated
policy server declares `languages: ["*"]` (to be a candidate everywhere,
which also puts it in every query's `languageServers` projection),
`forceStart: true` (below), and answers `enabled: false` for itself — a
pure routing authority that never receives a single document.

### The Query Point, Caching, and Staleness

Kakehashi queries at each routing decision: a host document's `didOpen`, and
each injection region's virtual-document creation. The answer is cached per
`(uri, languageId, configuration generation)` and invalidated by a
configuration reload and by replacement of the answering provider's
connection (restart or respawn — a new process may hold new policy).

Invalidation is **not retroactive**: it affects future decisions —
subsequent opens, and the derived re-open after a restart, which per
respawn-reopen-derives-its-targets derives against *current* state and
therefore re-consults routing — but never tears down routes already
established for an open document. Re-routing a live document is a
close/re-open, initiated by whoever holds the document, not by the bridge.

The round trip is bounded by a **routing timeout** — implementation-defined
with a documented default in the low-seconds class — and expiry is a `null`
answer with a warning, so a slow provider degrades the protocol to
kakehashi-decided routing, never blocks a document open indefinitely. The
request is an ordinary managed request for ls-bridge-timeout-hierarchy
purposes (unlike bridge-client-control-protocol's pass-through, its latency
class is known — bounded by this timeout — so it participates in normal
liveness accounting).

### Cold Start: `forceStart`

At the session's first `didOpen` no downstream server is running, so no
provider can answer, and pure lazy spawning would make the first documents
always route by kakehashi's defaults — defeating the provider a user
explicitly configured. Two pieces close the gap:

- **`languageServers.<name>.forceStart`** (new config; `None` = inherit
  from the wildcard, built-in default `false`): spawn this server eagerly
  when the session initializes, with no triggering document. It is a
  general warm-up knob, not a routing-specific flag — any latency-sensitive
  server benefits. With no document there is no marker walk, so the spawn
  uses the same root shape ls-bridge-server-pool-coordination gives a
  shared-instance re-seed: the client's primary root as a single seed, or
  a rootless spawn in a workspace-less session. `forceStart` is evaluated
  at startup and, for servers not already running, on configuration
  reload; it never overrides the stopped set (stopped > `forceStart`).
- **Bounded initialization wait**: a `priorities`-listed candidate that is
  still `Initializing` at query time is awaited — within the routing
  timeout — until its handshake settles and its advertisement can be read.
  Ready without the flag, or `Failed`: move to the next candidate.
  Advertisement is only observable after `initialize` completes, so
  without this wait a `forceStart`ed provider would lose exactly the
  first-open races it exists to win.

If no candidate is running or initializing, kakehashi decides — the
protocol adds no spawn of its own beyond `forceStart`.

## Considered Options

### A top-level `bridge.routing` config block

Rejected: the `bridge.<key>` slot is a language name (`bridge.python`,
`bridge._`), so `bridge.routing` reads as configuration for a language
called "routing". The aggregation method map already provides a
per-language and wildcard home with established inheritance.

### A new strategy name (`prioritized`)

Rejected: "ask in `priorities` order, first non-`null` wins" is exactly the
existing `preferred` strategy. A third vocabulary word for the same
dispatch would fragment the config surface.

### Unset `priorities` = routing disabled

Rejected: it would give this one method key a special default, breaking the
uniform `None → ["*"]` inheritance of aggregation-priorities-wildcard. The
advertisement gate already makes the permissive default free: servers that
do not advertise are never queried, so "no provider installed" and
"disabled" cost the same.

### Send the full server configuration in `languageServers`

Rejected: the raw `BridgeServerConfig` would freeze the config schema into
a wire contract and ship `cmd`, `initializationOptions`, and `settings` —
operational detail and potentially credentials — to every provider, none of
it routing signal. The `{languages, workspaceMarkers}` projection is
additive-evolvable and leaks nothing.

### `hostUri` on `textDocument`

Deferred, not rejected: providers routing an injection may want the host
document's identity, and bridge-client-control-protocol's `OpenDocument`
already models `uri` + optional `hostUri`. Adding the field later is purely
additive; v1 ships without it.

### Open the document on every `workspaceFolders` element's connection

For per-root servers, the override could be read as "open this document on
one connection per folder". Rejected: every aggregation, translation, and
document-lifecycle path assumes a document opens on at most one connection
per server name, and within-name response aggregation has no defined
semantics. First-element-primary preserves the invariant; the remaining
elements still warm connections for documents that will claim them.

### Editor-side routing via the bridge-client control protocol

The editor could answer routing questions instead of a downstream server.
Not chosen: the knowledge this protocol wants (project topology, ownership,
policy) lives project-side, where a server can compute it once for every
editor; an editor-side answer would need implementing in each client.
The two protocols compose rather than compete — an editor can still `stop`
what a routing provider enabled.

## Consequences

### Positive

- Routing becomes programmable per document without a kakehashi release:
  install a provider, it advertises, it works — zero configuration in the
  common case, `priorities` only to order multiple providers.
- Fail-open at every layer means the protocol can only ever *refine*
  behavior; its absence, failure, or misconfiguration reproduces today's
  routing exactly.
- Per-language provider policy falls out of the aggregation machinery for
  free, as does the ordered-allowlist vocabulary users already know.
- `forceStart` doubles as a general warm-up knob independent of routing.

### Negative

- One provider round-trip on the first routing decision per document (then
  cached). The advertisement gate and the routing timeout bound the cost,
  but a configured provider sits on the `didOpen` hot path by design.
- The bounded initialization wait trades first-open latency for routing
  correctness when a `forceStart` provider is still starting.
- A buggy provider silently thins routing: `enabled: false` answers leave
  no trace but warn-level logs. The per-document scope limits the blast
  radius (and the projection excludes config-disabled servers), but
  misrouting diagnostics is: read the logs.
- The `{languages, workspaceMarkers}` projection and the answer schema are
  new compatibility surfaces; both may evolve additively only.
- Providers will parse virtual URIs despite the opacity contract (Hyrum's
  law) — documented as unsupported, not prevented, same as
  bridge-client-control-protocol's ids.

### Neutral

- First kakehashi-*initiated* custom request: the `kakehashi/bridge/*`
  namespace now spans both directions (client→kakehashi control,
  kakehashi→downstream routing). Scope-first conventions are unaffected —
  the method shadows no LSP feature.
- First non-LSP method name in the `aggregation` key space; the map was
  always string-keyed, so this is a documentation fact, not a schema
  change.
- The `capabilities.experimental.kakehashi.*` discovery convention now has
  a downstream-facing instance, not only the client-facing one.

## Implementation Notes

- The advertisement is parsed from the initialize result and retained on
  the connection handle, in the same new per-handle slot
  bridge-client-control-protocol adds for `serverInfo`.
- The projection is a dedicated wire struct (camelCase serde, additive
  evolution), built from effective config after wildcard resolution — not
  a `serde` view of `BridgeServerConfig`.
- The routing query runs **before** the acquire critical section:
  decide-then-act. The answer (root override, enabled set) is an input to
  the acquire path; the query itself must never be awaited under the pool
  lock. Per-URI open serialization (the existing edit-lock discipline)
  keeps concurrent opens of one document from racing their queries.
- The cache is keyed `(uri, languageId, config generation)`; the
  provider-replacement invalidation hooks the pool's replacement-insertion
  commit point, where restart already serializes with settings
  publication.
- `forceStart` spawns ride the ordinary acquire path with a synthetic
  primary-root resolution, inside the same critical section that checks
  the stopped set and the control-operation registry — an eager spawn must
  lose the same races a lazy one loses.
- The bounded initialization wait subscribes to the handshake's terminal
  transition rather than polling; its deadline is the routing timeout, so
  a query never waits longer than it may run.
- Warm-connection establishment for non-primary `workspaceFolders`
  elements is fire-and-forget through the acquire path; failures log and
  do not affect the primary route.
- The routing method needs no entry on the pass-through deny list: a
  client forwarding `kakehashi/bridge/routing` via
  `kakehashi/bridge/client/request` merely asks the server the same
  question and corrupts no bridge state.

## Summary

| Aspect | Decision |
|---|---|
| **Method** | `kakehashi/bridge/routing`, kakehashi→downstream request |
| **Discovery** | initialize result `capabilities.experimental.kakehashi.bridgeRouting: true`; never queried without it |
| **Params** | `textDocument { uri, languageId }` (virtual URI for injections, contractually opaque) + `languageServers` projection `{languages, workspaceMarkers}` of enabled, language-matching servers |
| **Answer** | `null`/missing entry = kakehashi decides; `enabled: false` = per-document didOpen suppression; `workspaceFolders` = root-resolution override (`[]` = rootless) |
| **Precedence** | stopped set > routing answer > configuration |
| **Folders↔Key** | shared instance: set joins `WorkspaceFolderSet`; per-root: first element primary, rest warmed; document opens on one connection per server name |
| **Provider order** | `bridge.<lang>.aggregation."kakehashi/bridge/routing".priorities`, ordered allowlist, `preferred` dispatch, first non-`null` wins |
| **Failure** | timeout / error / malformed = `null` + warn; fail-open everywhere |
| **Caching** | per `(uri, languageId, config generation)`; invalidated by reload and provider replacement; never retroactive |
| **Cold start** | `languageServers.<name>.forceStart` (default `false`, primary-root seed) + bounded initialization wait within the routing timeout |
| **Recursion** | provider connections and queries never trigger routing queries |
