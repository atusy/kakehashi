# Bridge Routing Protocol

**Related Decisions**:
- [bridge-client-control-protocol](bridge-client-control-protocol.md) — the sibling `kakehashi/bridge/*` family (client→kakehashi); the `capabilities.experimental` discovery convention, the stopped set, and the liveness classification this protocol reuses
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the `ConnectionKey` model, per-root pooling, shared instances, and the workspace-folder capability fallback that a `workspaceFolders` override interacts with
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — the ordered-allowlist `priorities` semantics reused for provider selection
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — the `preferred` strategy whose fan-out/fan-in machinery this protocol dispatches with
- [ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md) — registers the routing decision deadline and the Tier-2 liveness exclusion
- [respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md) — the derived re-open, which consults only cached routing decisions
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — the inheritance resolution applied before the config projection goes on the wire
- [host-document-bridge](host-document-bridge.md) — the `_self` layer whose `enabled` gate and aggregation entry govern host-document routing decisions
- [language-server-bridge](language-server-bridge.md) — the security model this protocol's trust model extends

## Context

Routing — which downstream servers receive a document's `didOpen` and which
workspace root keys each connection — is decided entirely by static
configuration: `languages` membership, `workspaceMarkers` resolution,
`preferSharedInstance`, and the `enabled` flags
(`languageServers.<name>.enabled`, `bridge.<lang>.enabled`,
`bridge._self.enabled`). Nothing can decide routing *per document* from
project state: a monorepo tool that knows which sub-project owns a file, a
policy layer that enables a server only for directories that opt in, or a
meta-server that computes workspace topology cannot express any of that
through kakehashi's config alone.

bridge-client-control-protocol opened the connection pool to the editor-side
client. This decision opens the *routing policy* in the other direction:
kakehashi asks a downstream server how to route. The precedent for the shape
is `workspace/configuration` — the party holding the state is asked at the
moment the decision is needed — except here kakehashi is the asking client
and a downstream server is the answering authority.

## Decision

Introduce a kakehashi-initiated custom request, `kakehashi/bridge/routing`,
sent to downstream servers that advertise support. The answer refines
kakehashi's own routing decision for one (document, language) pair; every
absence — no provider, `null` answer, missing server entry, timeout, error —
falls back to kakehashi's existing behavior. The protocol is fail-open on
the transport: absence or failure of providers reproduces today's routing
exactly. A *successful* answer, by design, can only subtract servers or
redirect roots (see Trust Model).

### The Request

One request is issued per **(host document, language)** decision — not per
injection region. All injection regions of one language in one host document
route to the same connections, so the decision is genuinely per language;
querying per region would multiply one decision by the region count (the
shipped markdown injection query emits hundreds of `markdown_inline` regions
for an ordinary prose document) and key it to virtual URIs whose identity is
deliberately unstable across edits
(language-server-bridge-virtual-document-model).

```typescript
type RoutingParams = {
  // The document being routed: always the HOST document's real URI, paired
  // with the language under decision — the host's own languageId for the
  // host-layer decision, an injection languageId for injection decisions.
  textDocument: { uri: string; languageId: string };
  // The candidate set the decision is about: every configured server that
  // is spawnable (enabled, with an effective cmd) and whose effective
  // `languages` list matches `languageId` (a `"*"` element matches every
  // language — any-language-server-wildcard), projected to the
  // routing-relevant fields, with wildcard inheritance already resolved.
  // The `_` wildcard entry is a template, never a server, and is excluded.
  languageServers: Record<string, {
    languages?: string[];
    workspaceMarkers?: (string | string[])[];
    preferSharedInstance?: boolean;
  }>;
};

type RoutingResult = null | {
  routing: Record<string, {
    // Workspace folder URIs for this server. Absent or null = kakehashi
    // resolves the root as usual; non-empty = root-resolution override
    // (see below). An empty array is invalid in v1 (no rootless
    // ConnectionKey exists in a rooted session) and drops the entry.
    workspaceFolders?: string[] | null;
    // false = this document's didOpen is not forwarded to this server.
    // Absent = kakehashi decides; true = an explicit no-op affirmation.
    enabled?: boolean;
  }>;
};
```

- `textDocument` is deliberately not a bare `TextDocumentIdentifier` (the
  shape node-reference-protocol standardizes for custom methods): routing
  needs the effective languageId, which for injections is not derivable
  from the URI. The two-field shape mirrors the `uri` + `languageId`
  pairing of bridge-client-control-protocol's `OpenDocument` result type.
- `languageServers` is a **projection**, not the configuration. Only
  `languages`, `workspaceMarkers`, and `preferSharedInstance` are sent —
  the fields a routing decision can use (`preferSharedInstance` is what
  tells a provider whether a multi-folder answer joins one shared
  connection or is truncated to its first element — below) — after
  wildcard-config-inheritance resolution, so the provider sees effective
  values, not the sparse user file. `cmd`, `initializationOptions`, and
  `settings` are never sent: they carry no routing signal, `settings` is an
  opaque user value that may hold credentials, and shipping the raw config
  struct would freeze kakehashi's configuration schema into a wire
  contract. The projection is a compatibility surface of its own and
  evolves additively only.
- Servers that are not spawnable — effective `enabled: false`, or no
  effective `cmd` — are excluded from the projection entirely, so a
  provider cannot re-enable what configuration disabled: the record it
  would need to address is absent.

### Answer Semantics

The result layers a "kakehashi decides" default at every granularity:

- `null` result — kakehashi decides everything, exactly as without the
  protocol.
- A server missing from `routing`, or present with `enabled` absent —
  kakehashi decides that server's forwarding.
- `enabled: false` — the document's `didOpen` is not forwarded to that
  server for this language. Suppression is enforced at **candidate
  selection**, the one choke point every open path shares — the eager
  `didOpen`/parse fan-out, the lazy per-request `ensure_document_opened`,
  and the derived re-open sweep alike — so no later request can open the
  document there through a side door. This is **per-document forwarding
  suppression**, not slot control: the connection (if any) is untouched
  and other documents route normally. It is deliberately weaker than
  bridge-client-control-protocol's `stop`, which pins a whole slot.
- `enabled: true` — an explicit no-op: the server stays in kakehashi's
  hands, exactly as if the field were absent. It never overrides
  kakehashi's other gates (the per-host bridge filter, capability
  prefilters, spawnability); it exists so a provider can attach a
  `workspaceFolders` override while explicitly not suppressing.
- Malformed input is handled at three levels, all fail-open with a
  warning: a result that does not deserialize to `RoutingResult` at all is
  treated as `null`; a well-typed entry whose `workspaceFolders` fails
  validation (below) is dropped whole (kakehashi decides for that server);
  an entry naming an unknown server is ignored. An answer is also
  re-screened when applied: an entry whose server is no longer a current
  candidate (deleted, disabled, or no longer language-matching after a
  reload) is dropped with a warning.

**Precedence.** The stopped set of bridge-client-control-protocol outranks
everything; configuration decides membership; the answer only subtracts
from it or redirects roots:

**stopped set > configuration membership > routing answer.**

Concretely: a routing answer never resurrects a stopped slot, and the
stopped-set check covers **both** keys a routing answer can involve — the
key kakehashi's own resolution would produce *and* the key the
`workspaceFolders` override names. If either is stopped, the answer's entry
for that server is discarded (the configuration-resolved stop wins; a
provider cannot steer a document around a user's pin by naming a different
root). Without the double check, `stop` would otherwise be advisory against
a root-redirecting provider — the failure mode that decision's own
"auto-respawn" rejection exists to prevent.

### Trust Model

language-server-bridge's security model states that only explicitly
configured servers are ever spawned. This protocol keeps that invariant but
qualifies its spirit: a downstream process now influences routing. Stated
explicitly, a provider — which the user trusts by configuring its `cmd`,
like every other server — **can**:

- suppress any candidate server for any document it is asked about
  (silently, from the editor's point of view — see Consequences);
- redirect a server's workspace root within the validation bounds below,
  changing which directory tree that server indexes.

A provider **cannot**:

- cause any unconfigured command to run, or make a configured-but-disabled
  server spawnable (the projection excludes them, and unknown names are
  ignored);
- resurrect a stopped slot (precedence above);
- see `cmd`, `initializationOptions`, or `settings` (projection);
- point a server at an arbitrary filesystem location: each
  `workspaceFolders` element must be a `file:`-scheme URI and must either
  be an ancestor of the document's own path or lie within one of the
  client-announced workspace folders — the same universe marker walks and
  client roots already span. The element count per entry is capped
  (implementation-defined, documented default in the tens); an entry
  violating any of these rules is dropped whole with a warning.

Two disclosures are accepted and named: the projection reveals the user's
configured server names and marker chains to every provider queried, and
the `textDocument` URI reveals document paths. Both are the minimum the
question requires.

The remaining trust decision is **who may be a provider**: advertisement is
server-controlled, so under the default `priorities` (`["*"]`, below) a
server upgrade that adds the advertisement silently promotes an installed
server to routing authority with no user action. This is a deliberate
trade: requiring per-server opt-in would reintroduce exactly the
configuration burden the zero-config goal exists to remove, and the
authority granted stays inside the blast radius of a process the user
already chose to run on their code. Users who want an explicit gate get one
by naming providers: an explicit `priorities` list without `"*"` is a
provider allowlist.

### `workspaceFolders` and the `ConnectionKey` Model

A non-empty `workspaceFolders` overrides root resolution — the step where
`workspaceMarkers` would walk the document's ancestors — for this
(document, server) decision. The override maps onto
ls-bridge-server-pool-coordination's model as follows:

- **Shared instance** (`preferSharedInstance` honored, server capable):
  each folder joins the shared connection's `WorkspaceFolderSet` through
  the existing add-only announce path (`workspace/didChangeWorkspaceFolders`
  — one announce per element, or a batched entry point), and the document
  opens on the shared connection. The set is **union-only**: an override
  can widen the shared instance for every document on that server, never
  narrow it, and conflicts with folders other documents already added
  simply union. Removal remains the pool's separate idle-eviction
  follow-up.
- **Per-root connections** (by configuration, or via the capability
  fallback): the **first element** is the root — the document opens on
  `ConnectionKey::new(server, Some(first))`. Remaining elements are
  ignored with a warning: a per-root process serves one root, and opening
  the document on several same-name connections would break the invariant
  every aggregation and translation path assumes — a document opens on
  **at most one** connection per server name. (Pre-warming connections
  for the ignored elements was considered and deferred — below.) The
  projection's `preferSharedInstance` field exists precisely so a
  provider can predict which of these two readings its answer gets; the
  runtime capability fallback can still downgrade a shared-preferring
  server to per-root, in which case the same first-element truncation
  applies, warned.
- An empty array is **invalid in v1**: the rootless connection shape
  exists only in workspace-less sessions, and no rootless `ConnectionKey`
  variant exists in a rooted session's pool model. The entry is dropped
  with a warning (a rootless override is deferred — below).

Folder strings are workspace folder URIs; the folder `name` is the basename
of the root, exactly as `root_markers::workspace_at_root` derives it for a
marker-rooted spawn (falling back to the URI string for a root with no
basename).

### Discovery

A downstream server advertises support in its initialize **result**:
`capabilities.experimental.kakehashi.bridgeRouting: true`. This mirrors, on
the downstream-facing side, the convention bridge-client-control-protocol
set on the client-facing side, and like `serverInfo` there, the flag is
parsed from the initialize result and retained per connection handle.
Kakehashi in turn declares `experimental.kakehashi.bridgeRouting: true` in
the **client capabilities** it sends at `initialize`, so a provider can
skip building routing state under clients that will never ask.

Kakehashi never **initiates** a routing query to a server that did not
advertise — which is also what makes the default configuration free of
routing round trips: a fleet with no advertising server pays none. (A
client may still forward the method through
`kakehashi/bridge/client/request`; that is caller-driven traffic outside
the managed routing path, and needs no deny-list entry — it merely asks the
server the same question and corrupts no bridge state.) A server that
advertised but answers `MethodNotFound` (`-32601`) has its retained
advertisement cleared for the connection's lifetime, so a lying
advertisement stops costing round trips after the first.

Method dispatch is **per side**: the editor-facing custom methods
(`kakehashi/bridge/client/*` and the rest) are not registered on downstream
connections, and `kakehashi/bridge/routing` is not an editor-facing method
— a downstream server sending control-protocol requests back at kakehashi
is answered `MethodNotFound`, never dispatched.

### Provider Selection: Aggregation Machinery, `preferred` Dispatch

The **providers** asked and the **candidates** decided about are distinct
sets. Candidates (the projection) are the document's language-matching
spawnable servers. Providers are drawn from *all* configured spawnable
servers — a provider need not serve the document's language, which is what
lets a dedicated policy server opt out of ever being a document candidate
(below) — filtered to those that advertise `bridgeRouting` and are `Ready`
(or still `Initializing` within the decision deadline, below). Slots that
are `stopping`, `stopped`, `failed`, or mid-`restart`
(bridge-client-control-protocol's states) are skipped, never parked on.

Provider order is configured through the existing per-method aggregation
map — `languages.<host>.bridge.<lang>.aggregation`, abbreviated
`bridge.<lang>.aggregation` below as in its sibling ADRs — under the method
key `kakehashi/bridge/routing`:

```toml
[languages._.bridge._.aggregation."kakehashi/bridge/routing"]
priorities = ["policy-server", "*"]
```

An injection language's decision reads that language's `bridge.<lang>`
entry; the host document's decision reads the host-layer `bridge._self`
entry — and is therefore **gated on `bridge._self.enabled = true`**
(host-document-bridge's opt-in, off by default): with host bridging off
there are no host candidates and no host query. A language with no
candidates at all likewise queries nothing.

- `priorities` follows aggregation-priorities-wildcard: ordered allowlist,
  `"*"` for the unlisted rest, `None` inherits to `["*"]`, and an explicit
  `[]` disables routing queries for the language. For this method key the
  `"*"` expansion runs over all configured spawnable servers (the provider
  universe above), not the language's candidates.
- **The routing key does not inherit from the `"_"` method wildcard.**
  This is a deliberate exception to the aggregation map's field-level
  `"_"` merge: existing `aggregation."_".priorities` entries were written
  as LSP fan-out allowlists naming language servers, and folding them into
  the routing key would silently exclude every dedicated provider from
  routing for that language — a common config shape turning the protocol
  off with no signal. `bridge.<lang>` vs `bridge._` inheritance (the
  language axis) applies to the routing key exactly as to any other.
- The method consumes only `priorities`. `strategy`, `maxFanOut`,
  `pullFallback`, and `pushFallback` are not consumed — the request
  dispatches `preferred` regardless, the same posture
  language-server-bridge-request-strategies records for every method that
  does not consume a strategy. One asymmetry falls out and is accepted:
  `maxFanOut = 0` and `priorities = []` are recorded as equivalent for
  LSP methods (aggregation-priorities-wildcard), but for this key only
  `[]` disables — `maxFanOut = 0` does nothing.
- Dispatch reuses the `preferred` machinery as it actually works:
  **concurrent fan-out** to every selected provider, then the
  priority-aware fan-in picks the winner — the highest-priority answer
  that is non-`null` **and** has a non-empty `routing` map (the same
  non-empty rule `preferred` applies everywhere; `null`, `{ routing: {} }`,
  an error response, a timeout, and a malformed answer all mean "no
  opinion" and fall through to the next position). Named `priorities`
  entries are strict positions; within the `"*"` rest group the winner is
  the **earliest arrival**, not a ranking — deterministic provider
  ordering requires naming providers explicitly. The winning answer is
  attributed to the provider that produced it.
- Because the key space of `bridge.<lang>.aggregation` is method names, a
  per-language provider order works with no new machinery. This is the
  first non-LSP method in that key space.

Two structural rules keep the protocol from consuming itself:

- **No recursion.** Establishing a connection to a provider, and the
  routing query itself, never trigger a routing query. Provider
  connections are routed by kakehashi's own rules — the bootstrap base
  case. The derived re-open sweep after a restart consults only **cached**
  decisions (below), so a respawn never issues queries or spawns as a
  routing side effect — preserving respawn-reopen-derives-its-targets'
  read-only, never-spawns stage discipline and its fixed budget.
- **Document-independent traffic.** The routing request rides the
  provider's connection like any managed request but needs no open
  document. A provider that answers `enabled: false` for itself therefore
  keeps working: its documents are suppressed, its policy channel is not.

### The Query Point, Caching, and Staleness

Kakehashi queries at the **first routing decision** for a (host document,
language) pair: the host `didOpen` (when host bridging is enabled) and the
first virtual-document creation per injection language. The decision is
cached per `(host uri, languageId, config generation)` with **single-flight**
dedup — concurrent decision points for the same key await one in-flight
query rather than racing their own (no existing lock covers this: the
per-URI `edit_lock` is released before any bridge work begins, so the
protocol brings its own serialization).

Cache lifecycle:

- **Evicted on the host document's `didClose`** (all its languages'
  entries at once); an answer arriving for an already-closed document is
  discarded. The cache is therefore bounded by open documents × injection
  languages.
- **Flushed wholesale whenever the set of `Ready` advertising providers
  changes** — a provider reaching `Ready`, being replaced by restart or
  respawn, being stopped, or failing. The flush is deliberately coarse:
  the cache is small, per-entry provenance tracking is not worth its
  bookkeeping, and the rule uniformly covers the cases a finer rule
  mis-handles (a `null`-chained answer whose *losing* higher-priority
  provider restarts; a provider that was absent at query time appearing
  later). Fallback outcomes — no provider was available, or every
  provider declined — are cached like any answer and healed by the same
  flush when a provider arrives, so a cold-start document is re-decided
  on its next decision point rather than pinned to defaults for a
  generation.
- **Generation-anchored insert**: an in-flight query's answer is inserted
  only under the generation it was computed against; if a reload
  superseded it, the insert is invisible (one wasted round trip, never a
  wrong serve) — the same discipline the bridge's `cached_configs`
  snapshot cache records for exactly this race.
- **Generation revalidation at apply time**: the acquire path re-reads the
  current config generation inside its critical section and compares it
  to the answer's; on mismatch the answer is discarded and the acquire
  proceeds by kakehashi's own rules (fail-open). A stale answer is never
  applied across a reload.

Invalidation is **not retroactive**: it affects future decisions —
subsequent opens, re-opens, and the derived re-open after a restart (which
reads the cache) — but never tears down routes already established for an
open document. Re-routing a live document is a close/re-open, initiated by
whoever holds the document, not by the bridge. Providers have no push
channel to signal policy changes (deferred — below), so a decision can be
stale for a document's whole open lifetime; that is recorded as a
limitation, not an accident.

**The decision deadline.** One deadline — the **routing timeout**,
implementation-defined with a documented default in the low-seconds class,
registered in ls-bridge-timeout-hierarchy — bounds the *entire* decision:
all provider round trips (they run concurrently, so the bound is one
deadline, not a sum) and any initialization waits inside it. On expiry
kakehashi sends `$/cancelRequest` for every still-pending routing request,
retires their pending entries atomically with synthesizing the fallback
answer — a late response has nowhere to land and is dropped and logged —
and proceeds with kakehashi-decided routing plus a warning. Routing
requests are **excluded from Tier-2 liveness accounting**, exactly as
bridge-client-control-protocol excludes pass-through and via the same
per-entry classification: they carry their own deadline, and a slow
provider must degrade routing, never drive a `Ready` connection to
`Failed`. One consequence is worth naming: the decision is awaited on the
`didOpen` path, which holds the per-URI ingress writer ticket (the
mechanism that serializes same-document lifecycle messages in wire order),
so the routing timeout is also the bound on how long one document's
subsequent `didChange` traffic can stall behind its open.

### Cold Start: `forceStart`

At the session's first `didOpen` no downstream server is running, so no
provider can answer, and pure lazy spawning would make the first documents
route by kakehashi's defaults — defeating the provider a user explicitly
configured. Two pieces close most of the gap:

- **`languageServers.<name>.forceStart`** (new config; `None` = inherit
  from the wildcard, built-in default `false`, matching the
  `enabled`/`preferSharedInstance` inheritance shape): spawn this server
  eagerly, with no triggering document. It is a general warm-up knob, not
  a routing-specific flag — any latency-sensitive server benefits. The
  spawn fires **after the first effective configuration is published**
  (spawning at `initialize` would use a configuration the client's first
  settings push routinely replaces, evicting the fresh connection as pure
  churn), and rides the ordinary acquire path as a **get-or-create inside
  the acquire critical section** — with no document there is no marker
  walk, so it resolves the same marker-less fallback shape a document-less
  acquire produces (`root_markers::workspace_from_marker`: the
  client-supplied `rootUri` and the client's workspace-folder snapshot;
  rootless in a workspace-less session), and it observes the stopped set
  and control registry exactly as a lazy acquire does, colliding rather
  than double-spawning when one races it. It is re-evaluated on
  configuration reload for servers not already running under that key —
  the point where the stopped-set check has teeth, since the set is empty
  at session start. Because `didChangeConfiguration` layers accumulate
  (configuration-merging-strategy), `forceStart: true` persists until an
  explicit `false`, and a reload flipping it to `false` never stops an
  already-running server — within a session the flag is effectively
  one-way; `stop` is the lever that stops.
- **Bounded initialization wait**: a provider candidate still
  `Initializing` at query time is awaited **within the decision
  deadline** — advertisement is only observable once the handshake
  completes — by subscribing to the handshake's terminal transition. The
  subscription wakes on *any* exit from `Initializing`: `Ready` (read the
  advertisement; fan out if it is set), `Failed`, and `Closing` (a `stop`
  or teardown won the race — resolve to "no provider" immediately rather
  than burning the deadline). Global teardown resolves all waiting
  decisions to the fallback at once.

The wait is honest about its limits: it wins only against providers that
initialize within the low-seconds decision deadline. A heavy provider
(30-60 s initialization is the Tier-0 norm) still loses the first-open
races, after costing the full deadline — routing providers should be
lightweight, fast-initializing processes, and heavy servers that also act
as providers must expect the first opens to route by kakehashi's defaults
(healed by the provider-arrival cache flush above for documents whose next
decision point comes later).

If no candidate is running or initializing, kakehashi decides — the
protocol adds no spawn of its own beyond `forceStart`.

### The Dedicated Policy Server

Because provider candidacy is decoupled from document candidacy, a pure
routing authority is just:

```toml
[languageServers.policy-server]
cmd = ["my-policy-server"]
languages = []      # never a document candidate; never receives a didOpen
forceStart = true   # nothing else would ever spawn it
```

It is spawnable, advertises `bridgeRouting`, and is selected by `"*"` (or
named) in routing `priorities` — while `languages = []` keeps it out of
every projection, every eager open, and every fail-open fallback: even when
routing fails open, kakehashi's own rules never forward a document to it.
A server that is *both* a document server and a provider can still suppress
itself per document with `enabled: false`; its policy channel is unaffected
(document-independent traffic, above).

## Considered Options

### A top-level `bridge.routing` config block

Rejected: the `bridge.<key>` slot holds a language name (or the `_self`
host layer), so `bridge.routing` reads as configuration for a language
called "routing". The aggregation method map already provides a
per-language and wildcard home with established inheritance.

### A new strategy name (`prioritized`), or a sequential probe chain

Rejected: the dispatch this method needs — concurrent fan-out, walk
`priorities`, first non-empty answer wins — is exactly the existing
`preferred` machinery, reused as implemented. A *sequential* chain (ask,
wait, ask the next) was also considered and rejected: its worst case is the
sum of per-provider timeouts on the `didOpen` path, where concurrent
fan-out bounds the whole decision by one deadline.

### Unset `priorities` = routing disabled

Rejected: it would give this one method key a special default, breaking the
uniform `None → ["*"]` inheritance of aggregation-priorities-wildcard. The
advertisement gate keeps the permissive default free of round trips, and
the authority it grants advertising servers is confronted in the Trust
Model rather than hidden behind a default. (The routing key's exemption
from the `"_"` *method* wildcard is a narrower, different exception, made
so that pre-existing LSP fan-out allowlists cannot silently disable the
protocol.)

### Send the full server configuration in `languageServers`

Rejected: the raw `BridgeServerConfig` would freeze the config schema into
a wire contract and ship `cmd`, `initializationOptions`, and `settings` —
operational detail and potentially credentials — to every provider, none of
it routing signal. The three-field projection is additive-evolvable and
leaks nothing beyond server names and marker chains (Trust Model).

### Per-region queries with virtual-URI `textDocument`

An earlier draft queried per injection region, sending the region's virtual
URI. Rejected: all regions of one language in one host route identically,
so per-region queries multiply one decision by the region count — hundreds
for the shipped markdown injections — and virtual-URI identity is neither
stable across edits nor parseable by providers (it is contractually
opaque). The host URI is the identity project-side tooling can actually
reason about. A `hostUri` field is therefore also moot: the host URI *is*
the identity sent.

### Open the document on every `workspaceFolders` element's connection

For per-root servers, the override could be read as "open this document on
one connection per folder". Rejected: every aggregation, translation, and
document-lifecycle path assumes a document opens on at most one connection
per server name, and within-name response aggregation has no defined
semantics.

### Pre-warm connections for non-primary `workspaceFolders` elements

An earlier draft spawned a connection per ignored element so future
documents could find them warm. Deferred: it makes provider answers a
process-amplification vector (the pool has no idle eviction), contradicts
the "no spawn beyond `forceStart`" rule, and needs its own
generation/stopped-set race analysis — all for a speculative warm-up.
Ignoring the elements with a warning loses nothing correct.

### A rootless (`[]`) override

The draft defined `[]` as "no workspace folders". Deferred: the rootless
connection shape exists only in workspace-less sessions; a rooted session's
pool has no rootless `ConnectionKey` variant, and inventing one (with its
`Display` rendering, enumeration row, and stopped-set identity) is not
worth it before a concrete need exists.

### Provider-initiated invalidation

A `kakehashi/bridge/routing/invalidate` notification (downstream→kakehashi,
naming URIs or "all") would let the party that *knows* project state
changed say so, instead of relying on restart-as-invalidation. Deferred as
purely additive; until then, pull-only staleness is a recorded limitation.

### Routing-decision introspection

The routing answer's effects are invisible outside logs. An enumeration
hook riding the sibling control protocol (per-document "suppressed by
provider P" records, or a decisions query) is the obvious observability
follow-up. Deferred as purely additive.

### Editor-side routing via the bridge-client control protocol

The editor could answer routing questions instead of a downstream server.
Not chosen: the knowledge this protocol wants (project topology, ownership,
policy) lives project-side, where a server can compute it once for every
editor; an editor-side answer would need to be implemented in each client.
The two protocols compose rather than compete — an editor can still `stop`
what a routing provider enabled.

## Consequences

### Positive

- Routing becomes programmable per document without a kakehashi release:
  install a provider, it advertises, it works — zero configuration in the
  common case, `priorities` only to order or allowlist providers.
- Transport-level fail-open: no provider, a slow provider, a crashed
  provider, or a malformed answer all reproduce today's routing, at worst
  one decision deadline later.
- Per-language provider policy falls out of the aggregation machinery for
  free, as does the ordered-allowlist vocabulary users already know.
- `forceStart` doubles as a general warm-up knob independent of routing.
- The policy-server pattern needs no self-referential tricks: candidacy
  decoupling keeps a `languages = []` provider out of every document path
  by construction.

### Negative

- A configured provider sits on the `didOpen` hot path by design: the
  first decision per (document, language) costs concurrent round trips to
  every advertising provider, bounded by one routing deadline — and that
  deadline also bounds how long same-document `didChange` traffic can
  stall behind the open (the ingress writer ticket is held across the
  await).
- Fail-open bounds transport failure, not provider policy: a *successful*
  answer subtracts servers or redirects roots, and a buggy provider
  therefore silently thins routing. Until the deferred introspection
  hook exists, diagnosing a misroute means reading the warn-level logs.
- Routing is pull-only and non-retroactive: a document's decision can be
  stale for its whole open lifetime as project state moves under it; the
  provider has no way to say so (deferred invalidation notification).
- Under the default `priorities = ["*"]`, a server upgrade that adds the
  advertisement silently promotes an installed server to routing
  authority, and ordering between multiple `"*"`-group providers is
  arrival order, not deterministic — naming providers explicitly is the
  remedy for both.
- The three-field projection and the answer schema are new compatibility
  surfaces; both may evolve additively only.
- The bounded initialization wait cannot help providers that initialize
  slower than the low-seconds decision deadline; their first opens route
  by defaults after paying the full deadline.

### Neutral

- First kakehashi-*initiated* custom request: the `kakehashi/bridge/*`
  namespace now spans both directions (client→kakehashi control,
  kakehashi→downstream routing), with method dispatch strictly per side.
- First non-LSP method name in the `aggregation` key space; the map was
  always string-keyed, so this is a documentation fact, not a schema
  change — but the key's exemption from the `"_"` method wildcard is a
  real, documented irregularity.
- The `capabilities.experimental.kakehashi.*` discovery convention now has
  a downstream-facing instance and a client-capabilities instance, not
  only the editor-facing one.

## Implementation Notes

- The advertisement is parsed from the initialize result and retained on
  the connection handle, in the same new per-handle slot
  bridge-client-control-protocol adds for `serverInfo`; a `-32601` answer
  clears it for the connection's lifetime.
- The projection is a dedicated wire struct (camelCase serde, additive
  evolution), built from effective config after wildcard resolution — not
  a `serde` view of `BridgeServerConfig`.
- Suppression and root overrides apply at candidate selection
  (`get_host_configs_for_language` and the virt candidate enumeration),
  the choke point shared by the eager open fan-out, the lazy
  `ensure_document_opened` path, and the re-open sweep — not at
  individual `didOpen` call sites.
- The routing query is awaited before the acquire and holds no pool lock;
  the acquire critical section re-validates the answer's config
  generation (discard on mismatch) alongside its existing stopped-set and
  control-registry checks. The single-flight map and the decision cache
  are new state beside the pool's per-connection maps.
- Fan-out/fan-in reuse `fan_out` + the `preferred` collector over the
  expanded priority walk, with the routing-specific candidate universe
  (all spawnable configured servers, advertisement-filtered) and the
  non-empty rule applied to the `routing` map. Expiry cancellation uses
  `forward_cancel_downstream` per pending provider request; retirement
  must be atomic with the fallback synthesis so late answers drop.
- The Tier-2 exclusion rides the per-entry liveness classification the
  control protocol introduces for pass-through; routing entries carry the
  same non-liveness class.
- `forceStart` joins `KNOWN_BRIDGE_SERVER_SETTING_KEYS` (unknown-key
  allowlist) with a `forces_start()` accessor mirroring
  `prefers_shared_instance()`; its doc comment is user-facing config-schema
  hover output.
- The cache flush hook fires on `Ready`-set transitions of advertising
  servers: handshake completion, replacement insertion, stop, and
  failure. Each is already a pool-lock commit point; the flush is a
  synchronous map clear, safe to run inside them.
- ls-bridge-timeout-hierarchy gains the routing decision deadline
  (registered beside the per-slot control shutdown timeout) and the
  Tier-2 exclusion note; that edit lands with this ADR.

## Summary

| Aspect | Decision |
|---|---|
| **Method** | `kakehashi/bridge/routing`, kakehashi→downstream request; dispatch strictly per side |
| **Decision unit** | one query per (host document, language); `textDocument = { uri: host URI, languageId }` |
| **Params** | + `languageServers` projection `{languages, workspaceMarkers, preferSharedInstance}` of spawnable, language-matching servers (`_` excluded) |
| **Answer** | `null`/missing entry/absent `enabled` = kakehashi decides; `enabled: false` = per-document `didOpen` suppression at candidate selection; non-empty `workspaceFolders` = root override; `[]` invalid in v1 |
| **Precedence** | stopped set > configuration membership > routing answer (subtract/redirect only; both config-resolved and override keys checked against the stopped set) |
| **Trust** | providers are trusted-by-configuration; folder overrides bounded to `file:` URIs within the document's ancestry or client workspace folders, count-capped |
| **Folders↔Key** | shared instance: union-only join; per-root: first element, rest warned+ignored; at most one connection per server name per document |
| **Providers** | all spawnable configured servers ∩ advertising ∩ `Ready`/`Initializing`-within-deadline, ordered by routing `priorities` (no `"_"` method-wildcard inheritance); concurrent fan-out, `preferred` fan-in, non-empty rule |
| **Deadline** | one routing timeout per decision (low-seconds class, registered in ls-bridge-timeout-hierarchy); expiry cancels pending requests, retires entries, falls open; excluded from Tier-2 liveness |
| **Caching** | per (host uri, languageId, config generation); single-flight; evicted on `didClose`; wholesale flush on `Ready`-provider-set change; generation-anchored insert + apply-time revalidation; never retroactive |
| **Cold start** | `forceStart` (post-config-publication get-or-create, marker-less fallback root shape) + bounded initialization wait inside the decision deadline, woken by any handshake exit |
| **Recursion** | provider connections, queries, and the re-open sweep never trigger routing queries; re-open reads the cache only |
