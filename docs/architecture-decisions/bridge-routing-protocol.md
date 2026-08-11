# Bridge Routing Protocol

**Related Decisions**:
- [bridge-client-control-protocol](bridge-client-control-protocol.md) — the sibling `kakehashi/bridge/*` family (client→kakehashi); the `capabilities.experimental` discovery convention, the stopped set, and the liveness classification this protocol reuses
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the `ConnectionKey` model, per-root pooling, shared instances, and the workspace-folder capability fallback that a `workspaceFolders` override interacts with
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — the ordered-allowlist `priorities` semantics reused for provider selection
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — the `preferred` strategy whose fan-out/fan-in machinery this protocol dispatches
- [ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md) — registers the routing decision deadline and the Tier-1/Tier-2 exemptions
- [respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md) — the derived re-open, which consults only the active route binding
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — the inheritance resolution applied before the config projection goes on the wire
- [host-document-bridge](host-document-bridge.md) — the `_self` layer whose `enabled` gate and aggregation entry govern host-document routing decisions
- [language-server-bridge](language-server-bridge.md) — the security model this protocol's trust model extends
- [any-language-server-wildcard](any-language-server-wildcard.md) — the `"*"` language element and explicit-empty `languages` semantics the projection and the policy-server pattern rely on

## Context

Routing — which downstream servers receive a document's `didOpen` and which
workspace root each connection is keyed to — is decided entirely by static
configuration: `languages` membership, `workspaceMarkers` resolution,
`preferSharedInstance`, and the `enabled` flags
(`languageServers.<name>.enabled`, `bridge.<lang>.enabled`,
`bridge._self.enabled`). Nothing can decide routing *per document* from
project state: a monorepo tool that knows which sub-project owns a file, a
policy layer that enables a server only for directories that opt in, or a
meta-server that computes workspace topology cannot express any of that
through kakehashi's config alone. (An earlier decision attacked the same gap
with a per-server Lua hook; it is rejected, and its records deleted, as
this decision lands — see Considered Options.)

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
the transport: absence or failure of providers reproduces today's routing,
at worst one decision deadline later. A *successful* answer, by design, can
only subtract servers or redirect roots (see Trust Model).

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
  // Which decision this is. The host-layer and injection-layer decisions
  // are distinct even when languageId coincides (lua-in-lua,
  // markdown-in-markdown): they read different aggregation entries, sit
  // behind different gates, and cache separately.
  layer: "host" | "injection";
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
    // ConnectionKey exists in a rooted session) and the entry is dropped.
    workspaceFolders?: string[] | null;
    // false = this document's didOpen is not forwarded to this server.
    // Absent = kakehashi decides; true = an explicit no-op affirmation.
    enabled?: boolean;
  }>;
};
```

- `textDocument` is deliberately not a bare `TextDocumentIdentifier` (the
  shape node-reference-protocol standardizes for custom methods): routing
  needs the effective `languageId`, which for injections is not derivable
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
  evolves only additively.
- Servers that are not spawnable — effective `enabled = false`, or no
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
  server for this language. Suppression is enforced at the **routing
  gate**, a per-decision point every open path consults before sending
  any `didOpen` — the eager per-server open tasks, the lazy per-request
  open (the `ensure_document_opened` call on the request-execute path),
  and the derived re-open sweep alike. The applied decision is recorded
  as an **active route binding** that lives until the document closes
  (see Caching below), so no later request can open the document there
  through a side door for as long as it stays open (the gate's placement
  is specified under the deadline section). This is **per-document forwarding
  suppression**, not slot control: the connection (if any) is untouched
  and other documents route normally. It is deliberately weaker than
  bridge-client-control-protocol's `stop`, which pins a whole slot.
- `enabled: true` — a forwarding no-op: the server stays in kakehashi's
  hands, exactly as if the field were absent. It never overrides
  kakehashi's other gates (the per-host bridge filter, capability
  prefilters, spawnability); it exists so a provider can attach a
  `workspaceFolders` override while explicitly not suppressing. Its
  *presence* is not a no-op, however: an entry is **operative** for the
  fan-in predicate (below) when it carries `enabled` (either value) or a
  `workspaceFolders` value that is non-`null` and survives validation —
  so an affirmation-only answer still counts as an answer, while
  `workspaceFolders: null` alone, or an entry whose only content was
  dropped by validation, decides nothing.
- Malformed input is handled at three levels, all fail-open with a
  warning: a result that does not deserialize to `RoutingResult` at all is
  treated as `null`; a well-typed entry whose `workspaceFolders` fails
  validation (below) is dropped whole (kakehashi decides for that server);
  an entry naming an unknown server is ignored. An answer is also
  re-screened when applied: an entry whose server is no longer a current
  candidate (deleted, disabled, or no longer language-matching after a
  reload) is dropped with a warning.

**Precedence.** On the membership axis:

**stopped set > configuration membership > routing answer.**

The stopped set of bridge-client-control-protocol outranks everything;
configuration decides which servers exist as candidates; the answer only
subtracts from that set. On the **root axis** the answer *overrides*
configured resolution — replacing the `workspaceMarkers` walk is the
override's purpose — subject to the stopped-set rule and the Trust Model
bounds below.

Concretely: a routing answer never resurrects a stopped slot, and the
stopped-set check covers **both** keys a routing answer can involve — the
key kakehashi's own resolution would produce *and* the key the
`workspaceFolders` override names — for a per-root server, the key of the
override's first element, checked after the truncation below. If either is
stopped, the answer's entry for that server is discarded (the
configuration-resolved stop wins; a provider cannot steer a document
around a user's pin by naming a different root). For a shared-instance server the two keys are the same
root-independent `ConnectionKey::shared`, so the double check collapses
to one. Without it, `stop` would be advisory against a root-redirecting
provider — the failure mode that decision's own "auto-respawn" rejection
exists to prevent.

### Trust Model

language-server-bridge's security model states that only explicitly
configured servers are ever spawned. This protocol keeps that invariant but
qualifies its spirit: a downstream process now influences routing. Stated
explicitly, a provider — which the user trusts by configuring its `cmd`,
like every other server — **can**:

- suppress any candidate server for any document it is asked about
  (silently, from the editor's point of view — see Consequences);
- redirect a server's workspace root within the validation bounds below,
  changing which directory tree that server indexes;
- widen a shared-instance server's folder set for **every** document on
  that server, permanently for the session — the `WorkspaceFolderSet` is
  add-only and the pool has no idle eviction yet, so one answer about one
  file can grow what a shared server indexes until the session ends.

A provider **cannot**:

- cause any unconfigured command to run, or make a configured-but-disabled
  server spawnable (the projection excludes them, and unknown names are
  ignored);
- resurrect a stopped slot (precedence above);
- see `cmd`, `initializationOptions`, or `settings` (projection);
- point a server at an arbitrary filesystem location. Each
  `workspaceFolders` element must be a `file:`-scheme URI, and after
  canonicalization (symlinks resolved, on both sides) it must lie **at or
  below** one of: a client-announced workspace folder, or the root
  kakehashi's own resolution produces for that (document, server) — the
  marker walk's result or the client fallback root. Containment is
  component-wise, never string-prefix (`/proj/app` does not admit
  `/proj/app-secrets`). An override can therefore re-root a server within
  the universe kakehashi's own resolution already spans, never walk above
  it. A host document with a non-`file:` URI has no path of its own, so
  only the workspace-folder branch applies; a session that announced no
  workspace folders and resolves no root rejects every element. The
  element count per entry is capped (implementation-defined, documented
  default in the tens). An entry violating any of these rules is dropped
  whole with a warning.

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
by naming providers: an explicit `priorities` list without `"*"` — written
at the routing method key itself, which the `"_"` method wildcard does not
reach (below) — is a provider allowlist.

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
  narrow it, and folders other documents already added simply union in.
  Removal remains the pool's separate idle-eviction follow-up.
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
  applies, with a warning.
- An empty array is **invalid in v1**: the rootless connection shape
  exists only in workspace-less sessions, and no rootless `ConnectionKey`
  variant exists in a rooted session's pool model. The entry is dropped
  with a warning (a rootless override is deferred — below).

Folder strings are workspace folder URIs. Canonicalization is identity,
not merely a validation step: each element must resolve to an existing
**directory**, and the canonicalized URI — not the answer's original
spelling — is what flows everywhere downstream: the `ConnectionKey`, the
stopped-set checks, the route binding, the folder announce, and the
spawn. Two spellings of one directory (symlink, percent-encoding, case,
trailing slash) can therefore never mint two keys or slip past an
equivalent stopped key. The folder `name` is the basename of the
canonical root, exactly as `root_markers::workspace_at_root` derives it
for a marker-rooted spawn (falling back to the URI string for a root with
no basename).

### Discovery

A downstream server advertises support in its initialize **result**:
`capabilities.experimental.kakehashi.bridgeRouting: true`. This mirrors, on
the downstream-facing side, the convention bridge-client-control-protocol
set on the client-facing side, and like `serverInfo` there, the flag is
parsed from the initialize result and retained per connection handle.
Kakehashi in turn declares `experimental.kakehashi.bridgeRouting: true` in
the **client capabilities** it sends at `initialize`, unconditionally — a
configuration reload can enable routing mid-session without reinitializing
existing connections, so the declaration must not encode current config —
letting a provider skip building routing state only under clients that
genuinely never speak the protocol.

Kakehashi never **initiates** a routing query to a server that did not
advertise — which is also what makes the default configuration free of
routing round trips: a fleet with no advertising server pays none. (A
client may still forward the method through
`kakehashi/bridge/client/request`; that is caller-driven traffic outside
the managed routing path, and needs no deny-list entry — it merely asks the
server the same question and corrupts no bridge state.) A server that
advertised but answers `MethodNotFound` (`-32601`) has its retained
advertisement cleared — both the per-connection flag and the per-name
session memo that earns initialization waits (below) — so a lying
advertisement stops costing round trips, and waits, after the first.

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
(below) — filtered to those that advertise `bridgeRouting` and are `Ready`.
An `Initializing` server is awaited (within the decision deadline, below)
**only** when its advertisement is already known — observed earlier in the
session and remembered per server name — or it is named explicitly
(non-`"*"`) in the routing `priorities`: advertisement is observable only
after the handshake, and awaiting every initializing server to find out
would tax exactly the fleets that have no providers at all. Slots that are
`stopping`, `stopped`, `failed`, or mid-`restart`
(bridge-client-control-protocol's states) are skipped, never parked on. A
provider *name* can own several live connections at once (per-root
pooling, plus a `forceStart` spawn); the routing query rides exactly
**one** of them, picked by a total order over the name's handles —
shared, then client-fallback, then the remainder by ascending key
rendering — taking the first `Ready` handle, and, when the
initialization wait applies, awaiting the first `Initializing` handle in
that same order. An arbitrary but stable choice, recorded as such.

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
  off with no signal. The exception cuts both ways, and the inverse is
  the sharp edge: a *deliberate* global allowlist or kill switch written
  at `aggregation."_"` does not gate routing either — the only key that
  gates routing is `aggregation."kakehashi/bridge/routing"` itself, at
  any language level, wildcards included. Kakehashi warns once at config
  load when a `"_"` entry carries an explicit `priorities` list while the
  routing key is unset — the shape where intent and effect diverge.
  `bridge.<lang>` vs `bridge._` inheritance (the language axis) applies
  to the routing key exactly as to any other.
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
  that is non-`null` and non-empty under the caller-supplied predicate
  every `preferred` dispatch provides. Each answer is validated and
  normalized (Answer Semantics) **before** the predicate runs, so an
  answer whose only content validation dropped cannot win the walk and
  block lower providers. For routing the predicate is: the `routing` map
  holds at least one operative entry, per the operative rule above.
  `null`, `{ routing: {} }`, an entry with no fields, an error response,
  a timeout, and a malformed answer all mean "no opinion" and fall
  through to the next position. The priority walk's entries are **pruned
  to the selected provider set** before dispatch: a named entry whose
  server does not advertise or is not `Ready` drops out of the walk
  entirely, rather than sitting as a position no task will ever fill and
  stalling the fan-in until the whole set drains. Named `priorities`
  entries are strict positions; within the `"*"` rest group the winner
  is the **earliest arrival**, not a ranking — deterministic provider
  ordering requires naming providers explicitly. One consequence is
  deliberate and worth naming: any operative answer wins the *whole*
  decision, so a high-priority provider that answers only affirmations
  thereby vetoes every lower-priority provider — priority is authority.
  The decision resolves as soon as the priority walk can decide — every
  higher-priority position answered or failed and some position holding
  an operative answer — and at the latest when the last selected
  provider has answered or been skipped; the deadline is the outer
  bound, not a wait. The winning answer is attributed to the provider
  that produced it.
- Because the key space of `bridge.<lang>.aggregation` is method names, a
  per-language provider order works with no new machinery. This is the
  first non-LSP method in that key space.

Two structural rules keep the protocol from consuming itself:

- **No recursion.** Establishing a connection to a provider, and the
  routing query itself, never trigger a routing query. Provider
  connections are routed by kakehashi's own rules — the bootstrap base
  case. The derived re-open sweep after a restart consults only the
  **route binding** (below), so a respawn never issues queries or spawns
  as a routing side effect — preserving respawn-reopen-derives-its-targets'
  read-only, never-spawns stage discipline and its fixed budget.
- **Document-independent traffic.** The routing request rides the
  provider's connection like any managed request but needs no open
  document. A provider that answers `enabled: false` for itself therefore
  keeps working: its documents are suppressed, its policy channel is not.

### The Query Point, the Decision Cache, and the Route Binding

Kakehashi queries at the **first routing decision** for a (host document,
layer, language) tuple: the host `didOpen` (when host bridging is enabled)
and the first virtual-document creation per injection language. Two
structures with different lifetimes carry the outcome:

- The **decision cache** holds pre-application answers, keyed
  `(host URI, layer, languageId, config generation)` with **single-flight**
  dedup — concurrent decision points for the same key await one in-flight
  query rather than racing their own (no existing lock covers this: the
  per-URI `edit_lock` is released before the bridge's open fan-out
  begins, so the protocol brings its own serialization). A joiner
  inherits the in-flight decision's remaining deadline, never a fresh
  one. The cache is policy, and policy is invalidatable — flushed and
  evicted by the rules below.
- The **active route binding** records the decision's applied outcome:
  per (host document, layer, language), which servers were suppressed,
  which `ConnectionKey` each open landed on, and — for a shared-instance
  override — the canonical override folders themselves, all stamped with
  the document's open incarnation. The binding record is installed
  **synchronously at the query point, in a pending state**, and settles
  when the decision applies: a lazy request-path open that finds a
  *pending* binding awaits the in-flight decision (inheriting its
  remaining deadline) instead of default-opening — without that, a
  hover-class request racing the decision window could open the document
  on a server the arriving answer suppresses. The settled binding is
  *identity*, not policy: the lazy open and the derived re-open sweep
  consult it — a suppressed server stays suppressed, a bound key stays
  the key, and a shared replacement's re-open re-adds and announces the
  binding's folders before the `didOpen` (a `#shared` key carries no
  folders and a restart loses the old set, so the binding is the only
  place the override survives). It is evicted only by the host
  document's `didClose`, never by a flush — this is what makes
  invalidation non-retroactive without opening side doors: a flushed
  *cache* cannot lift a suppression or re-root an open document onto a
  second same-name connection, because those sites read the *binding*.
  Bindings are also **grandfathered against trust-universe shrinkage**:
  an override admitted under an earlier workspace-folder set keeps
  driving re-opens for that document's lifetime (the trust guarantee is
  scoped to admission time; revoking an open document's route is
  close/re-open, or `stop`). Only a server with no binding record at all
  (one that became a candidate after the open, say via reload) falls
  through to kakehashi's ordinary resolution.

Decision-cache lifecycle:

- **Evicted on the host document's `didClose`** — all its layers' and
  languages' entries, and its binding, at once. A configuration reload
  flushes the whole cache: superseded-generation entries are unreadable
  under the new generation and would otherwise sit resident until their
  document closes. Cache and binding are each bounded by open documents
  × layers × languages.
- **Flushed wholesale whenever the set of `Ready` advertising providers
  changes** — a provider reaching `Ready`, being replaced by restart or
  respawn, being stopped, failing, or having its advertisement cleared by
  `-32601` — **and whenever the effective client workspace-folder set
  changes** (`workspace/didChangeWorkspaceFolders`, or config paths that
  adjust it): the folder set is part of the trust universe, so *future*
  decisions must not reuse answers validated against the old set
  (existing bindings are grandfathered — above). The flush is
  deliberately coarse: the cache is small, per-entry provenance tracking
  is not worth its bookkeeping, and the rule uniformly covers the cases a
  finer rule mishandles (a `null`-chained answer whose *losing*
  higher-priority provider restarts; a provider that was absent at query
  time appearing later). Fallback outcomes — no provider was available,
  or every provider declined — are cached like any answer and healed by
  the same flush.
- **A flush affects only future decision points.** A document already
  open keeps its binding; a cold-start document that was decided by
  defaults is re-decided only at its next genuine decision point, which
  for an already-open document is close/re-open. Accepted: the
  alternatives are provider queries resurrecting on hover-class request
  paths, or a thundering herd of every open document re-deciding at once
  after a provider restart.
- **Triple anchor, checked at application.** A single-flight query
  captures (config generation, flush epoch, document open incarnation)
  when it starts — the epoch is a monotonic counter bumped by every
  flush; the incarnation is the per-open token the document store
  already mints — and its answer is **applied and inserted only while
  the anchors hold**, with a mismatch handled by kind:
  - a **generation or epoch** move (the document is still the same
    open) discards the flight's answer and the waiting open tasks fall
    open to kakehashi-decided routing — one wasted round trip, never a
    wrong serve — with one carve-out: an epoch bump caused solely by
    the `Ready` arrival of a provider **this flight selected and
    awaited** re-anchors the flight to the new epoch under its original
    deadline instead of discarding it (the arrival is the event the
    initialization wait exists for; without the carve-out the wait
    could never use the provider it awaited);
  - an **incarnation** move (`didClose`, or close/re-open) **aborts**
    the waiting tasks outright — no `didOpen` is sent at all, and the
    incarnation is re-checked at the open's enqueue commit point, so an
    old task can never emit a ghost open for a closed document. A
    re-opened document never receives the previous open's answer:
    the anchors are part of the flight's identity, so a caller arriving
    after a flush or re-open never joins the stale flight — it starts a
    fresh one.

  Generation anchoring alone — the discipline the bridge's
  `cached_configs` snapshot cache records for this race — covers
  neither a flush (which changes no generation) nor a re-open (which
  changes neither).
- **Generation revalidation at apply time**: the acquire path
  additionally re-reads the current config generation inside its
  critical section and compares it to the answer's; on mismatch the
  answer is discarded and the acquire proceeds by kakehashi's own rules
  (fail-open). A stale answer is never applied across a reload.

Invalidation is **not retroactive**: it affects future decisions —
subsequent opens, re-opens, and the derived re-open after a restart (which
reads the binding) — but never tears down routes already established for an
open document. Re-routing a live document is a close/re-open, initiated by
whoever holds the document, not by the bridge. Providers have no push
channel to signal policy changes (deferred — below), so a decision can be
stale for a document's whole open lifetime; that is recorded as a
limitation, not an accident.

**The decision deadline.** One deadline — the **routing timeout**,
implementation-defined with a documented default in the low-seconds class,
registered in ls-bridge-timeout-hierarchy — bounds the *entire* decision:
all provider round trips (they run concurrently, so the bound is one
deadline, not a sum) and any initialization waits inside it. A provider
reaching `Ready` with less than a minimum remaining budget
(implementation-defined floor) is skipped rather than queried into a
guaranteed timeout. On expiry kakehashi sends `$/cancelRequest` for every
still-pending routing request and retires those entries atomically with
synthesizing the fallback answer — a late response has nowhere to land and
is dropped and logged — then proceeds with kakehashi-decided routing plus a
warning. Routing requests are **excluded from Tier-2 liveness accounting**,
exactly as bridge-client-control-protocol excludes pass-through and via the
same per-entry classification: they carry their own deadline, and a slow
provider must degrade routing, never drive a `Ready` connection to
`Failed`. They are likewise **exempt from the Tier-1 per-request timeout**
(a routing fan-out is multi-server aggregation, which would otherwise fall
inside Tier-1's trigger once Phase 3 lands, and a 2-5s per-request bound
would preempt a longer routing deadline): the routing deadline is the sole
bound on these requests.

**Where the await lives.** The decision is *not* awaited on the `didOpen`
handler. The handler's candidate enumeration stays synchronous and the
per-URI ingress writer ticket stays await-free — the posture the open
path deliberately keeps (a slow await under the ticket wedges later
same-URI readers and writers; the codebase records exactly this hazard
for auto-install). Instead, the eager per-server open tasks — already
fire-and-forget off the ticket — share the decision: the first task to
consult the routing gate starts the single-flight query, its siblings
await the same future, and each task applies the answer (suppression,
root override, then its acquire) before sending its `didOpen`. The
injection-layer decision is awaited the same way by the virtual-document
open tasks, off the parse loop; the lazy request-path open and the
re-open sweep consult the binding only, as above. The deadline's cost is
therefore **deferred feature availability** — downstream opens, and the
features they enable, land up to one deadline later than today — not a
stalled writer ticket or parse cycle.

### Cold Start: `forceStart`

At the session's first `didOpen` no downstream server is running, so no
provider can answer, and pure lazy spawning would make the first documents
route by kakehashi's defaults — defeating the provider a user explicitly
configured. Two pieces close most of the gap:

- **`languageServers.<name>.forceStart`** (new config; `None` = inherit
  from the wildcard, built-in default `false`, matching the
  `enabled`/`preferSharedInstance` inheritance shape): spawn this server
  eagerly, with no triggering document. The spawn fires **after the first
  effective configuration is published** (spawning at `initialize` would
  use a configuration the client's first settings push routinely
  replaces, evicting the fresh connection as pure churn), and rides the
  ordinary acquire path as a **get-or-create inside the acquire critical
  section** — with no document there is no marker walk, so it resolves
  the same marker-less fallback shape a document-less acquire produces
  (`root_markers::workspace_from_marker`: the client-supplied `rootUri`
  and the client's workspace-folder snapshot; rootless in a
  workspace-less session), and it observes the stopped set and control
  registry exactly as a lazy acquire does, colliding rather than
  double-spawning when one races it. That fallback shape is also the
  honest scope of the warm-up: documents under marker roots resolve
  *marker* keys and will not reuse the warmed connection, so `forceStart`
  warms a usable connection only for shared-instance servers, marker-less
  workspaces, and the document-less policy server below — setting it on
  an ordinary per-root marker server pre-spawns a process most documents
  bypass, and kakehashi warns once at config load about that shape. It is
  re-evaluated on configuration reload for servers not already running
  under that key — the point where the stopped-set check has teeth, since
  the set is empty at session start. Because `didChangeConfiguration`
  layers accumulate (configuration-merging-strategy), `forceStart = true`
  persists until an explicit `false`, and a reload flipping it to `false`
  never stops an already-running server — within a session the flag is
  effectively one-way; `stop` is the lever that stops.
- **Bounded initialization wait**: a provider whose advertisement is
  known (or that is explicitly named in `priorities` — the filter above)
  and that is still `Initializing` at query time is awaited **within the
  decision deadline** by subscribing to the handshake's terminal
  transition. The subscription wakes on *any* exit from `Initializing`:
  `Ready` (fan out if the advertisement holds), `Failed`, and `Closing`
  (a `stop` or teardown won the race — resolve to "no provider"
  immediately rather than burning the deadline). Global teardown resolves
  all waiting decisions to the fallback at once.

The wait is honest about its limits: it wins only against providers that
initialize within the low-seconds decision deadline. The Tier-0
initialization timeout is 30-60s precisely because heavy servers can take
that long to index — such a server still loses the first-open races after
costing the full deadline. Routing providers should be lightweight,
fast-initializing processes; heavy servers that also act as providers must
expect the first opens to route by kakehashi's defaults, healed only at
each document's next decision point (close/re-open, per the cache rules
above).

If no provider is running or initializing, kakehashi decides — the
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
An explicit `[]` overrides any `languages` a `languageServers._` wildcard
would supply (explicit-empty is documented configuration, not an error), so
a `_`-level `languages = ["*"]` cannot pull the policy server back into
candidacy. A server that is *both* a document server and a provider can
still suppress itself per document with `enabled: false`; its policy
channel is unaffected (document-independent traffic, above).

## Considered Options

### A per-server `workspaceResolver` Lua function

An earlier decision (workspace-resolver, with its lua-host-api companion
surface) attached a sandboxed, user-authored Lua function to each server
entry, returning per-document `(attach, workspace)` with the document text
in hand. **Rejected; both records are deleted as this decision lands**
(delete-on-supersede; the Lua resolver and its host API were never
implemented — the `rootMarkers` → `workspaceMarkers` rename that ADR also
carried had already shipped, and stays). It answers the same gap —
content-dependent per-document attach and rooting — but from the wrong
side, on two counts. Authoring: the resolver is program code embedded in a
TOML string — no syntax highlighting, no formatter, no linting, no unit
tests, escaping rules layered on top of Lua's own — and every user writes
and maintains those per-server scripts inside kakehashi's configuration.
Weight: kakehashi itself grows an embedded Lua runtime — an `mlua`
dependency, a stripped sandbox, a worker-thread timeout model, and a
curated `kakehashi.*` host API (the whole lua-host-api surface) — all of
it existing for that one hook. A routing provider moves the same logic
into a long-lived project-side process that tooling can ship, version, and
test independently of any user's config, decides across all servers in one
answer, and adds no runtime to kakehashi. One narrowing is accepted and
recorded: the resolver saw the unsaved buffer (`document_info.text`),
while `RoutingParams` carries no document text and the query precedes the
`didOpen` — a provider decides from project state and document identity,
reading disk, never unsaved content.

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
sum of per-provider timeouts on the `didOpen` path, whereas concurrent
fan-out bounds the whole decision by one deadline.

### Unset `priorities` = routing disabled

Rejected: it would give this one method key a special default, breaking the
uniform `None → ["*"]` inheritance of aggregation-priorities-wildcard. The
advertisement gate keeps the permissive default free of round trips, and
the authority it grants advertising servers is confronted in the Trust
Model rather than hidden behind a default. (The routing key's exemption
from the `"_"` *method* wildcard is a narrower, different exception, made
so that pre-existing LSP fan-out allowlists cannot silently disable the
protocol — at the recorded cost that deliberate `"_"` restrictions do not
reach it either.)

### Send the full server configuration in `languageServers`

Rejected: the raw `BridgeServerConfig` would freeze the configuration
schema into a wire contract and ship `cmd`, `initializationOptions`, and
`settings` — operational detail and potentially credentials — to every
provider, none of it routing signal. The three-field projection is
additive-evolvable and leaks nothing beyond server names and marker chains
(Trust Model).

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

An earlier draft defined `[]` as "no workspace folders". Deferred: the
rootless connection shape exists only in workspace-less sessions; a rooted
session's pool has no rootless `ConnectionKey` variant, and inventing one
(with its `Display` rendering, enumeration row, and stopped-set identity)
is not worth it before a concrete need exists.

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
a slot a routing provider left in play.

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
- The policy-server pattern needs no self-referential tricks: candidacy
  decoupling keeps a `languages = []` provider out of every document path
  by construction.
- `forceStart` doubles as a warm-up knob for shared-instance servers and
  the policy-server pattern (its scope limits are recorded in the
  decision).

### Negative

- A configured provider defers first feature availability by design: the
  first decision per (document, layer, language) costs concurrent round
  trips to every selected provider, bounded by one routing deadline,
  and the downstream opens that decision gates land up to that deadline
  later than today. (The ingress writer ticket and the parse loop are
  deliberately not stalled — the await lives in the fire-and-forget open
  tasks.)
- Fail-open bounds transport failure, not provider policy: a *successful*
  answer subtracts servers or redirects roots, and a buggy provider
  therefore silently thins routing. Until the deferred introspection
  hook exists, diagnosing a misroute means reading the warn-level logs.
- Routing is pull-only and non-retroactive: a document's decision can be
  stale for its whole open lifetime as project state moves under it; the
  provider has no way to say so (deferred invalidation notification),
  and an open document keeps its route binding until close/re-open even
  when a newer provider would decide differently.
- Under the default `priorities = ["*"]`, a server upgrade that adds the
  advertisement silently promotes an installed server to routing
  authority, and ordering between multiple `"*"`-group providers is
  arrival order, not deterministic — naming providers explicitly is the
  remedy for both.
- The three-field projection and the answer schema are new compatibility
  surfaces; both may evolve only additively.
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
  real, documented irregularity, in both directions.
- The `capabilities.experimental.kakehashi.*` discovery convention now has
  a downstream-facing instance and a client-capabilities instance, not
  only the editor-facing one.

## Implementation Notes

- The advertisement is parsed from the initialize result and retained on
  the connection handle, in the same new per-handle slot
  bridge-client-control-protocol adds for `serverInfo`; a `-32601` answer
  clears it for the connection's lifetime. The "advertisement known"
  memo that gates initialization waits is per server name, session-scoped.
- The projection is a dedicated wire struct (camelCase serde, additive
  evolution), built from effective config after wildcard resolution — not
  a `serde` view of `BridgeServerConfig`.
- Candidate enumeration (`get_host_configs_for_language` in the
  coordinator, and the virt candidate enumeration) stays synchronous.
  Suppression and root overrides apply at the routing gate: one shared
  per-decision future awaited inside the per-server open tasks; the lazy
  per-request open (`ensure_document_opened`, a pool function called
  from the request-execute path) and the re-open sweep consult the route
  binding, awaiting it while pending — one gate, not scattered
  per-call-site checks, and never an await under the ingress ticket.
- The routing query is awaited inside the open task, before that task's
  acquire, holding no pool lock; the acquire critical section
  re-validates the answer's config generation (discard on mismatch)
  alongside its existing stopped-set and control-registry checks. The
  single-flight map (anchored (generation, flush epoch, open
  incarnation) — the incarnation is the per-open token the document
  store's open tracker already mints), the decision cache, and the
  active route binding are new state beside the pool's per-connection
  maps; the binding rides the same per-document lifecycle the
  open-incarnation tracker maintains.
- Fan-out/fan-in reuse `fan_out` + the `preferred` collector over the
  expanded priority walk, with the routing-specific provider universe
  (all spawnable configured servers, advertisement-filtered) and the
  operative-entry predicate supplied as the non-empty check. One
  routing-specific step precedes dispatch: the expanded entries are
  pruned to the selected provider set (`expand_priorities` does not do
  this, and the `maxFanOut` truncation step is deliberately skipped).
  Expiry cancellation uses `forward_cancel_downstream` per pending
  provider request; retirement must be atomic with the fallback
  synthesis so late answers drop.
- The Tier-2 exclusion rides the per-entry liveness classification the
  control protocol introduces for pass-through; routing entries carry the
  same non-liveness class.
- `forceStart` joins `KNOWN_BRIDGE_SERVER_SETTING_KEYS` (unknown-key
  allowlist) with a `forces_start()` accessor mirroring
  `prefers_shared_instance()`; its doc comment is user-facing config-schema
  hover output.
- Two config-load advisories ship with the feature: a `"_"` aggregation
  entry carrying an explicit `priorities` list while the routing key is
  unset (restriction the user likely believes covers routing), and
  `forceStart = true` on a per-root marker server with non-empty
  `languages` (a warm-up most documents will bypass).
- The cache flush hook fires on `Ready`-set transitions of advertising
  servers: handshake completion, replacement insertion, stop, failure,
  and `-32601` advertisement clearing. Each is already a pool-lock commit
  point; the flush (a map clear plus an epoch bump) is synchronous, safe
  to run inside them.
- ls-bridge-timeout-hierarchy gains the routing decision deadline
  (registered beside the per-slot control shutdown timeout) and the
  Tier-2 exclusion note; that edit lands with this ADR.

## Summary

| Aspect | Decision |
|---|---|
| **Method** | `kakehashi/bridge/routing`, kakehashi→downstream request; dispatch strictly per side |
| **Decision unit** | one query per (host document, layer, language); `textDocument = { uri: host URI, languageId }` + `layer` |
| **Params** | `textDocument` + `layer` + `languageServers` projection `{languages, workspaceMarkers, preferSharedInstance}` of spawnable, language-matching servers (`_` excluded) |
| **Answer** | `null`/missing entry/absent `enabled` = kakehashi decides; `enabled: false` = per-document `didOpen` suppression at the routing gate; non-empty `workspaceFolders` = root override; `[]` invalid in v1 |
| **Precedence** | membership: stopped set > configuration > answer (subtract only); root: answer overrides marker resolution, both resolved keys checked against the stopped set |
| **Trust** | providers are trusted-by-configuration; folder overrides bounded to canonicalized `file:` URIs at-or-below client workspace folders or the config-resolved root, count-capped |
| **Folders↔Key** | shared instance: union-only join; per-root: first element, rest warned+ignored; at most one connection per server name per document |
| **Providers** | all spawnable configured servers ∩ advertising ∩ `Ready` (Initializing awaited only when the advertisement is known or the server is named), ordered by routing `priorities` (no `"_"` method-wildcard inheritance); concurrent fan-out, `preferred` fan-in, operative-entry rule |
| **Deadline** | one routing timeout per decision (low-seconds class, registered in ls-bridge-timeout-hierarchy); expiry cancels pending requests, retires entries, falls open; exempt from Tier-1 and Tier-2 accounting; awaited in the open tasks, never under the ingress ticket |
| **Caching** | decision cache per (host URI, layer, languageId, config generation), single-flight, evicted on `didClose`, flushed on reload / `Ready`-provider-set / workspace-folder-set change, (generation, flush-epoch, open-incarnation)-anchored; applied outcomes live in a per-document **route binding** until `didClose`; never retroactive |
| **Cold start** | `forceStart` (post-config-publication get-or-create, marker-less fallback root shape, warm-up scope limited to shared/marker-less/policy servers) + bounded initialization wait inside the decision deadline, woken by any handshake exit |
| **Recursion** | provider connections, queries, and the re-open sweep never trigger routing queries; re-open reads the binding only |
