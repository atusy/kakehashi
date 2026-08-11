# Bridge Routing Protocol

**Related Decisions**:
- [bridge-client-control-protocol](bridge-client-control-protocol.md) — the sibling `kakehashi/bridge/*` family (client→kakehashi); the `capabilities.experimental` discovery convention, the stopped set, and the liveness classification this protocol reuses
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — the `ConnectionKey` model, per-root pooling, shared instances, and the workspace-folder capability fallback that a `workspaceFolders` override interacts with
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — the ordered-allowlist `priorities` semantics reused for provider selection
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — the `preferred` strategy whose fan-out/fan-in machinery this protocol dispatches
- [ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md) — registers the routing decision deadline, the binding-reuse validation budget, and the Tier-1/Tier-2 exemptions
- [ls-bridge-async-connection](ls-bridge-async-connection.md) — the framing size ceilings (amended with this decision) that the answer-allocation bound depends on
- [respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md) — the derived re-open, which issues no routing query and never spawns: bound server entries answer from their binding, entry-less servers from marker resolution
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
kakehashi's own routing decision for one (document, layer, language)
tuple; every
absence — no provider, `null` answer, missing server entry, timeout, error —
falls back to kakehashi's existing behavior. The protocol is fail-open on
the transport: absence or failure of providers reproduces today's
*initial* routing decision, at worst one decision deadline later — with
one recorded difference in what happens afterwards: the decision settles
into the document's route binding either way, so even a fallback route is
frozen for the **binding's lifetime** (the host's close; an injection
tuple's last-region disappearance — with the one abnormal-finalization
exception the Caching section records), where today a restart's re-open
re-resolves live markers (respawn-reopen-derives-its-targets records the
same freeze from its side). A *successful* answer, by design, can
only subtract servers or redirect roots (see Trust Model).

### The Request

One **decision** — one logical fan-out, carrying one JSON-RPC request per
selected provider — is issued per **(host document, layer, language)**,
not per injection region. All injection regions of one language in one host document
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
  as an **active route binding** that lives for the binding's lifetime
  — until the host's `didClose` for the host layer; until `didClose` or
  the language's last region disappearing for an injection tuple (see
  Caching below) — so no later request can open the document there
  through a side door while the binding lives (the gate's placement
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
  an entry naming an unknown server is ignored. The answer is also
  bounded *before* normalization: the number of `routing` entries
  examined is capped at the size of the projection the query carried
  (`languageServers`) — a discoverable-by-construction limit a complete
  honest answer can never exceed, with unknown names consuming the same
  budget — and an answer beyond it is discarded whole as malformed
  (`null` + warn), so a provider cannot make kakehashi walk unboundedly
  many entries it would only discard.
  The *allocation* bound cannot live at this layer — the frame is fully
  read before method dispatch — so a framing-level ceiling on
  downstream message size, with defined oversized-frame behavior, is
  recorded as a transport hardening this bound depends on
  (Implementation Notes). An answer is also
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
around a user's pin by naming a different root). Both checks run
**inside the route-admission critical section** — realized by the
acquire's own critical section for routed entries, and by the same pool
lock for acquire-less suppressions — never as a separate
look-then-admit, which a concurrently committing `stop` could slip
between — and the binding retains **both** keys, so
every later binding-driven acquire (the lazy open, the re-open sweep)
re-checks both in its own critical section: a key stopped after
admission makes the bound route not applicable, exactly as ordinary
resolution against a stopped key opens nothing. For a shared-instance
server the two keys are the same root-independent
`ConnectionKey::shared`, so the double check collapses to one. Without it, `stop` would be advisory against a root-redirecting
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
  it. Only `file:`-scheme, existing-directory anchors contribute to the
  universe — a non-`file:` client-fallback root cannot be canonicalized
  or compared component-wise and contributes **no** anchor. A host
  document with a non-`file:` URI has no path to walk, so the marker
  branch never contributes either; its universe is the client workspace
  folders plus the fallback root *when that root is a `file:`
  directory*, and a session where no anchor qualifies rejects every
  element. The
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
equivalent stopped key. Canonicalization is admission-time work, and
binding-driven *reuse* does not trust it forever: **every** binding-driven
acquire or open — a retained-key retry, a restart's folder re-add, and
the fast paths that find an already-live per-root handle or an
already-announced folder — **re-canonicalizes** the retained
**override-originated** URI (an ordinarily resolved root reuses
verbatim — it is kakehashi's own value, per the binding's identity
contract) and requires the result to **equal** the admission-time
canonical URI
(containment is not enough: a directory swapped for a symlink to a
*sibling* inside the same trust root still canonicalizes elsewhere, and
equality catches it where containment would not). A mismatch reads not
applicable, with a warning. The residual window is named honestly: like
any path-based check, a swap landing between revalidation and the OS
call that uses the path narrows to a race, not to zero — closing it
outright needs filesystem-identity enforcement (device/inode pinning),
recorded here as an implementation option, not a v1 requirement. The folder `name` is the basename of the
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
advertisement cleared — the per-connection flag always, and the per-name
session memo that earns initialization waits (below) once no other live
handle of that name still advertises — so a lying advertisement stops
costing round trips, and waits, after the first.

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
(non-`"*"`) in the routing `priorities`, or it carries
`forceStart = true` (an explicitly configured eager spawn is as strong a
signal of intent as a named priority — without this, a first `didOpen`
racing startup would freeze fallback routing past the very provider
`forceStart` exists to have ready): advertisement is observable only
after the handshake, and awaiting every *other* initializing server to
find out would tax exactly the fleets that have no providers at all. Slots that are
`stopping`, `stopped`, `failed`, or mid-`restart`
(bridge-client-control-protocol's states) are skipped, never parked on. A
provider *name* can own several live connections at once (per-root
pooling, plus a `forceStart` spawn); the routing query rides exactly
**one** of them, picked from two ordered sets under one total order —
shared, then client-fallback, then the remainder by ascending key
rendering. The query goes to the first handle that is **`Ready` and
advertising**; when none exists and the initialization wait applies, the
wait targets the first **wait-eligible `Initializing`** handle in the
same order (an `Initializing` handle cannot advertise yet — its
eligibility comes from the per-name memo, an explicit `priorities`
entry, or `forceStart`). An arbitrary but stable choice, recorded as
such.

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
  block lower providers. Normalization is **per entry**: suppression
  and affirmation entries need no filesystem work and normalize
  immediately, while a folder-bearing entry whose validation cannot
  complete within the decision's remaining budget (or acquire pool
  capacity) is dropped with a warning — the answer's other, already
  normalized entries stand, and the predicate runs on whatever
  normalization produced by the deadline. Folder-entry validations
  launch **concurrently** onto the pool — concurrency removes serial
  dependency between entries; capacity can still gate them, per the
  caveat below. Admission is an online **priority queue**, not
  arrival-order FIFO: among jobs simultaneously awaiting a permit, the
  canonical key (the answering provider's priority-walk position, then
  server name, then provider name as the total tie-breaker within a
  `"*"` group) decides who is admitted next — a later-arriving
  higher-position answer sorts ahead of *waiting* jobs but never
  recalls running ones, so nothing is buffered against the budget
  waiting for answers that may never come, JSON object order confers
  nothing, and `"*"` fan-in's earliest-arrival semantics are untouched
  (this order governs validation admission only). The canonical key
  orders jobs **within one decision**; the shared pool arbitrates
  **across** decisions — and the binding-reuse validations, which have
  no provider or position and form their own queue — by fair
  round-robin over the per-decision queues, and a queue at its bound
  refuses the enqueue, which reads as that entry's capacity drop. "Arrival" for fan-in
  purposes is **completed normalization**, not raw receipt: an answer
  becomes eligible as a whole once every entry has validated or
  dropped — a suppression-only answer normalizes instantly and so
  completes at receipt, while an earlier raw response still validating
  can legitimately lose the `"*"` group's earliest-arrival race to a
  later answer that finished first; answers never enter fan-in
  entry-by-entry — and the non-starvation
  claim is scoped honestly: concurrency removes *serial* dependency
  between entries, while pool *capacity* can still gate them (a hung
  validation retains its permit), in which case entries denied
  capacity within budget drop per the capacity rule. Within
  normalization, whole-answer discard is reserved for an answer that
  never arrived, never deserialized, or failed the structural bounds
  (the entry cap); application-anchor invalidation (the triple-anchor
  rules) is its own, later discard path. For routing the predicate is: the `routing` map
  holds at least one operative entry, per the operative rule above.
  `null`, `{ routing: {} }`, an entry with no fields, an error response,
  a timeout, and a malformed answer all mean "no opinion" and fall
  through to the next position. The priority walk's entries are **pruned
  to the selected provider set** before dispatch — where "selected"
  spans both the dispatchable (`Ready` and advertising) and the
  **wait-eligible `Initializing`** providers: a wait-eligible position
  stays in the walk until its bounded wait terminates (it may yet
  answer), and only an entry that is neither dispatchable nor
  wait-eligible drops out, rather than sitting as a position no task
  will ever fill and stalling the fan-in until the whole set drains. Named `priorities`
  entries are strict positions; within the `"*"` rest group the winner
  is the **earliest arrival**, not a ranking — deterministic provider
  ordering requires naming providers explicitly. One consequence is
  deliberate and worth naming: any operative answer wins the *whole*
  decision, so a high-priority provider that answers only affirmations
  thereby vetoes every lower-priority provider — priority is authority.
  The decision resolves as soon as the priority walk can decide — every
  higher-priority position **normalization-complete** (its answer fully
  validated or dropped, or the provider failed; a raw answer still
  validating is not exhausted, since its folder entries may yet
  normalize into the higher-priority operative result) and some
  position holding an operative normalized answer — and at the latest
  when the last selected provider's answer has completed normalization
  or been dropped or skipped; the deadline is the outer bound, not a
  wait. The winning answer is attributed to the provider
  that produced it.
- Because the key space of `bridge.<lang>.aggregation` is method names, a
  per-language provider order works with no new machinery. This is the
  first non-LSP method in that key space.

Two structural rules keep the protocol from consuming itself:

- **No recursion.** Establishing a connection to a provider, and the
  routing query itself, never trigger a routing query. Provider
  connections are routed by kakehashi's own rules — the bootstrap base
  case. The derived re-open sweep after a restart **issues no routing
  query and never spawns as a routing side effect** — it reads each
  exact (host, layer, language, server) entry's binding record where
  one exists and falls through to read-only marker resolution where
  that entry is absent (below) — preserving
  respawn-reopen-derives-its-targets' read-only, never-spawns stage
  discipline and its fixed budget.
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
  which `ConnectionKey` each open landed on, and — for **every** shared
  routed/retained entry, override-driven or not — the effective
  folder(s) **selected for the route and persisted before the acquire
  runs** (`retained` entries have folders even though their acquire
  failed and nothing was announced), all stamped with the document's
  open incarnation. The retained value's identity contract differs by
  origin: an **override** folder is provider-supplied and retains its
  admission-time canonical URI under the Trust Model's full
  validation; an **ordinarily resolved** root is kakehashi's own value
  and is retained **exactly as resolution produced it** — no
  canonicalization, no directory requirement, no decision-budget
  charge, and reuse compares it verbatim (a canonical filesystem
  identity beside it is an optional hardening, not a requirement) —
  so a fallback decision needs no override-style validation or
  canonicalization (the marker walk's own metadata reads are the
  ordinary cost it always had) and non-`file:` client roots remain
  representable. Retaining the folder for
  non-override shared routes too is what gives a restarted shared
  replacement a folder source for every bound document; live resolution
  serves only a (host, layer, language, server) entry with no record —
  a terminally deleted entry falls through even while sibling entries
  stay settled. The pending binding record is
  installed **synchronously in the `didOpen` handler (respectively the
  virtual-document creation path), before the open tasks are spawned
  and before the writer ticket is released** — installing it inside the
  first spawned task would leave a scheduling gap in which a request
  running after the handler returns sees no record at all. The flight —
  the single-flight future and its deadline clock — is created
  **atomically with the pending record**, so an observer of a pending
  entry always has a future to await and a remaining budget to inherit.
  The flight is driven by a **dedicated driver task** spawned with it;
  every other party — the open tasks, the lazy open, the re-open sweep —
  is a passive subscriber, so an observer polling the future can never
  be the one to initiate provider I/O (in particular, the query-free
  sweep guarantee cannot be broken by the sweep merely awaiting a
  pending binding). Every server's
  entry then reaches a **terminal settlement or terminal deletion**
  (deletion is logged with per-server provenance like any settlement),
  so waiters never hang:
  *suppressed* settles when the answer applies — no acquire runs, but
  the settlement still commits inside a **route-admission critical
  section** under the same pool lock the acquires use, where the
  both-key stopped check, the config-generation revalidation, and the
  current-candidacy re-screen all run atomically with the commit
  (uniform with routed entries, whose admission section is the
  acquire's own — a stale suppression is never applied across a reload
  either);
  *routed(key)* settles at that server's acquire commit, recording the
  key actually landed on (a capability-fallback downgrade records the
  downgraded per-root key); *retained(key)* settles when the decided
  route's acquire failed — or, under abnormal finalization, was never
  attempted because its owner died; the terminal record carries
  which — waiters proceed without the
  server, and later opens retry the retained key through the ordinary
  respawn path rather than falling through to a different resolution.
  Retries are **single-flight per entry**: a retry claims the entry
  under the admission lock with a **unique claim token fenced by the
  open incarnation** (a byte-identical retained value recreated by a
  close/re-open can never satisfy an old claimant's commit — the token
  and incarnation must both match), concurrent would-be retriers await
  and consume the claimant's published outcome under their own
  budgets, and a commit whose token or incarnation moved writes
  nothing — a late failed retry can never regress a `routed` entry.
  The claim does not change what consumers see: the entry remains
  observably *retained* (its key and folders keep answering
  enumeration, the sweep, and lazy lookups), with the claim token as
  bookkeeping beside it. The claim is covered by the same pool-owned
  completion guard as every detached routing task: any exit short of a
  commit — cancellation, validation timeout, panic, teardown —
  restores plain *retained* **through the same token-and-incarnation
  CAS the claimant would use**: on a match it restores and publishes;
  on a mismatch (the entry moved — close/re-open, eviction, another
  transition) it leaves the current state untouched and only releases
  the old claim's waiters. Either way a dead claimant can never park
  later retriers or overwrite a successor's state. A
  successful retry transitions the entry to *routed* with the key the
  acquire actually landed on; a failed retry re-records
  *retained(actual attempted key)* with fresh provenance — after a
  capability downgrade that is the downgraded per-root key, initial
  attempt or retry alike, never a stale `#shared` name, with the
  route's folders preserved — so enumeration and later retries always
  follow the key really being attempted;
  *not-applicable* settles when a server is genuinely rejected
  **before** any acquire runs — a configuration removal or disablement
  that ends its candidacy for the document — and is consumed like a
  suppression (waiters proceed without the server; distinct provenance
  in the logs). The winning answer is logged in two stages, because its
  effects are not all known at once: a **decision record** when the
  fan-in decides — the answering provider, the (document, layer,
  language), and the normalized directives, including an
  affirmation-only win, which vetoes every lower-priority provider
  while changing nothing and would otherwise leave no trace — and a
  **terminal record** per server, emitted on settlement and terminal
  deletion alike (the landed or
  downgraded key; a retention, with its failed-versus-owner-died
  provenance; a suppression taking hold; a candidacy rejection's
  not-applicable; a deletion, with its
  cause — no surviving directive, or the retryable mismatch that
  triggered it). This
  is what makes a valid-but-wrong policy diagnosable from the logs
  (warnings cover the rejected, invalid, and timed-out outcomes; a
  *successful* misroute would otherwise be silent). Method-level capability prefilters are deliberately
  *not* a cause — they are per-request facts (lacking hover does not
  mean lacking completion) and never settle the binding. It is **not** the
  generation-mismatch outcome: a mismatch whose server is still a
  current candidate falls to ordinary resolution, whose acquire commit
  settles *routed(key)*/*retained(key)* as usual, and a decision that
  ends with no answer settles the same way — every settled entry
  carries a key, a suppression, or a not-applicable, so a lazy waiter
  can never resolve a different key than the eager open landed on, and
  no entry stays pending forever. Every settlement write is a
  **compare-and-set against the exact pending (incarnation, flight) it
  settles**: a task outlived by a close/re-open finds the new
  incarnation's record and writes nothing. A lazy request-path open that finds a *pending* entry
  awaits its settlement (bounded by the decision's remaining deadline)
  instead of default-opening — without that, a hover-class request
  racing the decision window could open the document on a server the
  arriving answer suppresses. The settled binding is
  *identity*, not policy: the lazy open and the derived re-open sweep
  consult it — a suppressed server stays suppressed, a bound key stays
  the key, and a shared replacement's re-open re-adds and announces the
  binding's folders before the `didOpen` (a `#shared` key carries no
  folders and a restart loses the old set, so the binding is the only
  place the override survives). If the replacement no longer supports
  workspace folders — the capability fallback downgraded it — the bound
  shared route is **not applicable**: the sweep neither opens with
  unannounced folders nor silently re-roots to a per-root key; the
  document's features on that server stay dark until close/re-open runs
  a fresh decision under the new capability reality. It is evicted only
  at the end of its lifetime — the host's `didClose`, or an injection
  tuple's last-region disappearance — never by a flush — this is what makes
  invalidation non-retroactive without opening side doors: a flushed
  *cache* cannot lift a suppression or re-root an open document onto a
  second same-name connection, because those sites read the *binding*.
  Bindings are also **grandfathered against trust-universe shrinkage**:
  an override admitted under an earlier workspace-folder set keeps
  driving re-opens for the binding's lifetime (the trust guarantee is
  scoped to admission time; revoking an open document's route is
  close/re-open — or the tuple's language leaving — or `stop`). Only a server with no binding record at all
  (one that became a candidate after the open, say via reload) falls
  through to kakehashi's ordinary resolution. The binding governs
  **every** site that derives a connection from the host URI — the
  resolve envelopes (`completionItem/resolve`, `codeAction/resolve`,
  `codeLens/resolve` — every envelope that re-derives a connection from
  a host URI) included, whose re-resolution would otherwise reach the
  config-root process instead of the one that produced the item
  (ls-bridge-server-pool-coordination is amended accordingly). The
  envelope must carry enough identity to *hit the right binding*: the
  layer/language pair, the open incarnation, and the tuple's **binding
  generation** — drawn from one **session-global** monotonic 64-bit
  counter that is never reset and never reused (no per-host or
  per-tuple allocator state exists at all, so nothing survives or
  resets across `didClose`; monotonicity rules out collision and a
  64-bit space cannot wrap in practice), stamped anew each time a
  tuple's binding is created, because an injection language can leave
  and re-enter the document without the host incarnation moving —
  matched exactly: after
  a close/re-open or a tuple re-creation the same URI holds a *new*
  binding, and an unstamped stale item would silently resolve through
  it. A resolve whose stamp no longer matches — the
  binding evicted, or a newer incarnation in its place — fails soft as
  an unroutable envelope does today.

Decision-cache lifecycle:

- **Evicted on the host document's `didClose`** — all its layers' and
  languages' entries, and its binding, at once — **and per injection
  language when its last region disappears**: a parse supersession that
  removes an injection language from the document evicts that (host,
  injection, language) tuple's cache entry and binding and retires its
  pending flight through the same cleanup path a `didClose` uses, so a
  flight whose subscribers vanished with their regions still reaches a
  terminal state, and a document that churns through language ids does
  not accumulate tuples for its whole open lifetime. "Disappears" means
  an **authoritative parse result without the language** — transient
  tree-less states (a reload-invalidation placeholder, a parse failure,
  a missing parser) preserve bindings, and the parse-state
  discriminator that distinction requires, which the re-open decision
  records as not existing today, is an implementation prerequisite of
  this eviction rule. The eviction commits as a **non-inserting
  compare-and-set** against the exact incarnation/content version the
  parse result speaks for and the tuple's current binding generation —
  a lagging parse of older text can never evict a language the newest
  text retains or has reintroduced. A configuration reload
  flushes the whole cache: superseded-generation entries are unreadable
  under the new generation and would otherwise sit resident until their
  document closes. Cache and binding are each bounded in top-level keys
  by open documents × layers × languages; entries in **both** carry
  per-candidate-server payloads (a normalized answer's routing map, a
  binding's settlements) and capped folder lists, so the byte bound
  multiplies by the candidate-server count and the folder cap. The
  framing ceilings bound the *answer-originated* strings; server-name
  bytes are proportional to their configuration sources (a
  file-originated name is bounded by its file's read ceiling; names
  from session overrides by that ingress), and host-URI bytes are
  proportional to upstream URI lengths, which no protocol ceiling
  bounds — the same proportionality every per-URI structure in the
  server already carries, not a new exposure.
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
  for an already-open document is close/re-open — or, for one injection
  language, its authoritative disappearance and re-entry. Accepted: the
  alternatives are provider queries resurrecting on hover-class request
  paths, or a thundering herd of every open document re-deciding at once
  after a provider restart.
- **Triple anchor, checked at application.** A single-flight query is
  anchored on (config generation, flush epoch, document open
  incarnation) — the epoch is a monotonic counter bumped by every
  flush; the incarnation is the per-open token the document store
  already mints. The incarnation and **config generation** anchors are
  fixed when the pending record is created — the projection and
  candidate set are built from that generation, so a reload landing
  before dispatch invalidates the flight rather than blessing a stale
  projection with a fresh generation; only the flush-epoch anchor is
  captured at **provider enumeration** — one pool critical section
  shared by the epoch read and the handle walk, run by every flight
  including one that selects nobody — so provider-set churn before
  enumeration folds into the selection instead of invalidating a
  flight that never dispatched, and a provider cannot commit `Ready`
  between the read and the walk and land selected-under-E+1 while the
  flight anchors to E. An **empty selection** validates the same
  anchor at its fallback commit: a provider reaching `Ready` in the
  gap bumps the epoch, the commit misses, and the flight
  re-enumerates under its remaining deadline — the arrival that
  should turn an empty decision into a queried one does exactly that
  instead of being sealed out. The driver re-verifies its exact pending (incarnation, flight) and
  generation **before dispatching** — a close or reload landing before
  its first scheduling cancels it without any provider I/O, and
  `didClose`/teardown signal that cancellation directly.
  The answer is **applied and inserted only while the anchors hold**,
  with a mismatch handled by kind:
  - a **generation or epoch** move (the document is still the same
    open) discards the flight's answer and the waiting open tasks fall
    open to kakehashi-decided routing — one wasted round trip, never a
    wrong serve — with one carve-out: an epoch bump caused solely by
    the `Ready` arrival of a provider **this flight selected and
    awaited** re-anchors the flight to the new epoch under its original
    deadline instead of discarding it (the arrival is the event the
    initialization wait exists for; without the carve-out the wait
    could never use the provider it awaited). The carve-out is
    enforceable because every flush advances the epoch **exactly once
    and records its cause** — for a Ready transition, the exact handle
    — so a flight re-anchors only when each advance since its anchor is
    a recorded Ready transition of a handle it awaited; any other
    cause, another provider's arrival or a workspace-folder change,
    discards as usual. The bookkeeping is a **bounded cause ring**, not
    a log and not a per-flush walk: the epoch-bumping critical section
    appends one (epoch, cause, handle) record in O(1) — never touching
    the live flights, whose count scales with open documents — and a
    flight re-anchors by reading only the causes in its own anchor gap,
    under the same lock that registers, re-anchors, and settles
    flights. The ring's capacity is an implementation-defined small
    constant; a flight whose gap has overflowed the ring — or whose
    provenance is missing for any reason — discards, never re-anchors;
  - an **incarnation** move (`didClose`, or close/re-open) **aborts**
    the waiting tasks outright — no `didOpen` is sent at all, and the
    open's enqueue commit re-checks the incarnation **and, for a
    virtual open, the tuple's current binding generation and the exact
    current region/virtual-document identity** (the incarnation alone
    does not move when an injection language leaves and re-enters, and
    the tuple generation alone does not move when one region of a
    language is removed while another remains). The enqueue
    registration and the tuple's eviction/close share **one lifecycle
    critical section** — an old task cannot validate, lose the lock to
    a cleanup that evicts and closes everything, and then register a
    ghost open. So a stale task can never open a closed document or a
    removed region. A
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

Invalidation is **not retroactive**: it affects only *new* decisions —
first opens and close/re-opens (for an injection language, also its
leaving and re-entering the document); the derived re-open after a
restart reads the binding and is untouched by cache invalidation — and
never tears down routes already established for an open document.
Re-routing a live document is a close/re-open — or, for one injection
language, an edit that removes and re-introduces it — initiated by
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
still-pending routing request and retires those entries, drops the
folder entries still stuck in validation (per-entry, as normalization
defines), and runs the fan-in over whatever normalized results exist —
a completed suppression or affirmation from an answered provider still
wins its position; the **whole** fallback is synthesized, atomically
with the retirements so a late response has nowhere to land, only when
no operative normalized result remains — then proceeds, warned. Routing requests are **excluded from Tier-2 liveness accounting**,
exactly as bridge-client-control-protocol excludes pass-through and via the
same per-entry classification: they carry their own deadline, and a slow
provider must degrade routing, never drive a `Ready` connection to
`Failed`. They are likewise **exempt from the Tier-1 per-request timeout**
(a routing fan-out is multi-server aggregation, which would otherwise fall
inside Tier-1's trigger once Phase 3 lands, and a 2-5s per-request bound
would preempt a longer routing deadline): the routing deadline is the sole
bound on these requests.

Filesystem validation — the canonicalization above, **and** the
ordinary marker resolution an override entry needs to construct its
trust anchor and its configuration-resolved stopped key (marker walks
make synchronous metadata calls that a network or automounted path can
stall) — runs off the async executor on a **globally bounded validation
pool** (a semaphore with a
bounded queue), charged against the caller's remaining budget; at the
deadline the affected entry drops (per-entry, as normalization defines)
and the orphaned blocking call's result
is discarded — the *decision's* latency stays bounded even where an OS
filesystem call (a network mount, an automount) cannot be cancelled and
outlives it detached, and a hanging mount can pin at most the pool,
never an unbounded worker pile: a caller that cannot acquire pool
capacity within its budget fails open without launching another orphan.
Binding-*reuse* revalidation carries its own bound — the sweep's
remaining budget on sweep paths, and a dedicated, implementation-defined
validation budget on lazy-open and retained-key retry paths (registered
in ls-bridge-timeout-hierarchy) — and "exceeded" splits by caller: a
lazy or retry attempt skips the server for that attempt, warned; the
**sweep** reports the host **applicable-but-unsettled** — the barrier's
fail-soft path — never a successful omission, because a bound key that
merely timed out validating is uncertainty, not non-membership. Permit
ownership makes the pool bound real: the semaphore permit travels
**into** the blocking task and is released only when the OS call
returns — a timed-out *waiter* abandoning the call must not free
capacity for the next caller to hang another worker — so permanently
hung calls can exhaust the pool for unrelated providers. That is the
recorded failure mode, and its effects are the contract's, not a
blanket fail-open: a new decision's *folder-bearing* entries fail open
(root overrides need validation), while suppression-only entries need
no filesystem work and stay effective; override-bound reuse that cannot
validate stays dark and sweep barriers fail soft until capacity
returns. Teardown never blocks on the pool.

Because they carry no tier accounting, outstanding provider requests are
cleaned up on **every terminal outcome of the decision**, not only
deadline expiry: an early winner (fan-in decided while lower-priority
requests were still pending), an incarnation abort, global teardown, and
expiry alike cancel every still-pending routing request
(`$/cancelRequest`) and retire its router entry atomically with the
decision's settlement — a hung losing provider can never pin an entry
indefinitely, and a late answer always finds its entry gone. The same
terminal outcomes also **remove the decision's queued validation jobs**
and discard their waiters and eventual results; only an
uninterruptible filesystem call already running stays detached, with
its permit, until it returns. The
driver and the per-server open tasks are additionally covered by a
**pool-owned completion guard** of the same class the control protocol
requires for its detached operations, with the two roles split. An
abnormal exit of the **driver** runs the same algorithm deadline
expiry runs: cancel and retire the outstanding provider requests, drop
validations still unfinished, fan-in over the **completed normalized**
results (raw or half-validated answers never count), and synthesize
the fallback only when no operative normalized result remains. An
abnormal exit of one **open task** finalizes only that task's own
(incarnation, flight, server) *entry*, never discarding a
still-running or already-decided shared flight. Entry finalization is
by the stage the dead task reached, and it commits **through the same
route-admission critical section as the live path** — the locked
candidacy, generation, and both-key stopped checks included, so an
abnormal exit cannot grandfather a directive a reload or `stop` has
since rejected: a decided suppression commits as *suppressed*; a
decided route settles *retained(key)* — later opens retry the key; a
**candidacy** rejection settles *not-applicable* exactly as the live
path would (deletion would let a later re-add resurrect what the live
rule freezes); and an entry whose task died **before any directive was
persisted** does not settle unilaterally while the shared flight still
lives — the guard's adoption check and the flight's terminalization
share **one locked handshake**: under that lock the guard either finds
the flight non-terminal and transfers the entry to it (the flight's
outcome then settles it like any other entry — deleting it early would
let a lazy waiter ordinary-open a server the flight's answer is about
to suppress), or finds the flight already terminal and finalizes the
entry immediately from that outcome; a driver settling concurrently
scans owned entries under the same lock, so no entry can slip between
adoption and settlement ownerless. Deletion — a terminal event, not a
silent removal: waiters subscribed to the entry are woken with "retry
as absent", atomically with the removal, and the absent-record
semantics (ordinary resolution, lazy retry) apply — happens only once
the flight has terminally established that no **route-affecting**
directive (no suppression, no override — an affirmation-only win
included) applies to the entry, or on the retryable generation/stopped
mismatches. This is the one **recorded narrowing of the freezing
guarantee**, and its scope is exactly the deletion rule above: an
entry whose dead task left a surviving route-affecting directive still
settles (*suppressed*, or *retained(key)*); only an entry with no such
directive — or a retryable mismatch — ends absent, and absence
promises no single frozen route for **that server's entry** (the
tuple's other entries keep their settlements): subsequent retriers run
ordinary resolution, whose outcome can vary with live filesystem
state, and the read-only sweep consuming the absence establishes
nothing; the entry stays unbound until some later open's admission
settles a record, exactly as if routing had never spoken for that
server. The guard settles records; it never
performs opens or acquires. Cancellation ownership is **exclusively
the driver/flight side's**: open tasks are passive subscribers and
their guard touches no provider request; the flight's cleanup honors
ls-bridge-message-ordering's queued-versus-sent distinction — work
still queued at the writer is atomically marked for writer-side skip,
work writing-or-sent gets its `$/cancelRequest`, either before
retirement, never a bare "not written" test a queued item could race
past. A (handle, id) registration is attached to the flight guard's
cleanup ownership **atomically with the router insertion**, so a panic
between the two cannot leak an unretained registration.

**Where the await lives.** The decision is *not* awaited on the `didOpen`
handler. The handler's candidate enumeration stays synchronous and the
per-URI ingress writer ticket stays await-free — the posture the open
path deliberately keeps (a slow await under the ticket wedges later
same-URI readers and writers; the codebase records exactly this hazard
for auto-install). Instead, the handler installs the pending record and
spawns the flight's dedicated driver (both synchronous), and the eager
per-server open tasks — already fire-and-forget off the ticket — await
the shared future as passive subscribers, each applying the answer
(suppression, root override, then its acquire) before sending its
`didOpen`. The
injection-layer decision is awaited the same way by the virtual-document
open tasks, off the parse loop; the lazy request-path open and the
re-open sweep consult the binding per exact server entry — falling
through to ordinary (for the sweep, read-only marker) resolution where
an entry is absent — and never query, as above. The deadline's cost is
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
  workspace-less session) — **except** for a `preferSharedInstance`
  server, where it resolves the `#shared` key directly, seeded with the
  client's primary root exactly as the control protocol's shared
  re-seed is (the document-less acquire's ordinary answer would be the
  client-fallback key, which marker-rooted documents bypass — the
  warm-up would warm a process nothing uses); the capability verdict
  lands at the handshake as for any shared spawn, and an incapable
  server simply serves nothing new, per the existing fallback. It
  observes the stopped set and control registry exactly as a lazy
  acquire does, colliding rather than double-spawning when one races
  it. That fallback shape is also the
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
  known, that is explicitly named in `priorities`, or that carries
  `forceStart` (the filter above), and that is still `Initializing` at
  query time is awaited **within the
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
  provider, or a malformed answer degrade toward today's *initial*
  routing decision, at worst one decision deadline later — and expiry
  is partial-result, so another provider's operative normalized answer
  still wins its position rather than being discarded with the slow
  one (the resulting binding, fallback or not, still freezes the route
  for the binding's lifetime — the Decision section's recorded
  difference, with the abnormal-finalization exception it names).
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
  hook exists, diagnosing a misroute means reading the logs — the
  apply-time records for successful effects, the warnings for
  rejected ones.
- Routing is pull-only and non-retroactive: a document's decision can be
  stale for its whole open lifetime as project state moves under it; the
  provider has no way to say so (deferred invalidation notification),
  and an open document keeps its route binding until close/re-open (an
  injection tuple: until its language leaves the document; the
  abnormal-finalization exception aside) even when a newer provider
  would decide differently.
- Under the default `priorities = ["*"]`, a server upgrade that adds the
  advertisement silently promotes an installed server to routing
  authority, and ordering between multiple `"*"`-group providers is
  arrival order, not deterministic — naming providers explicitly is the
  remedy for both.
- The three-field projection and the answer schema are new compatibility
  surfaces; both may evolve only additively.
- The bounded initialization wait cannot help providers that initialize
  slower than the low-seconds decision deadline; their first opens
  decide without them after paying the full deadline (by another
  provider's answer when one exists, by defaults otherwise).

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
  binding, awaiting a pending entry bounded by the lesser of the
  decision's remaining deadline and (for the sweep) its own fixed
  budget — still pending at that bound is **applicable-but-unsettled**,
  never "not applicable" (that outcome is reserved for documents that
  do not belong — respawn-reopen-derives-its-targets), so the
  execute-command barrier takes its fail-soft path rather than
  releasing over a document the settling binding may yet restore; the
  ordinary lazy path re-opens later. One gate, not scattered
  per-call-site checks, and never an await under the ingress ticket.
- The dedicated driver owns the provider query; each open task only
  awaits the shared decision future before its own acquire, holding no
  pool lock while waiting; the acquire critical section
  re-validates the answer's config generation (discard on mismatch) and
  checks the stopped set for **both** retained keys — the target and the
  configuration-resolved one — alongside its existing control-registry
  checks, in that one critical section. The
  single-flight map (anchored (generation, flush epoch, open
  incarnation) — the incarnation is the per-open token the document
  store's open tracker already mints), the decision cache, and the
  active route binding are new state beside the pool's per-connection
  maps; the binding rides the per-document lifecycle the
  open-incarnation tracker maintains, with the injection-tuple
  last-region eviction layered on top (the binding-generation counter
  is session-global, not per-document state).
- Fan-out/fan-in reuse `fan_out` + the `preferred` collector over the
  expanded priority walk, with the routing-specific provider universe
  (all spawnable configured servers, advertisement-filtered) and the
  operative-entry predicate supplied as the non-empty check. One
  routing-specific step precedes dispatch: the expanded entries are
  pruned to the selected provider set (`expand_priorities` does not do
  this, and the `maxFanOut` truncation step is deliberately skipped).
  Routing request ids are bridge-minted like every downstream id
  (each handle's `next_request_id` allocator — the pool ADR's old
  upstream-id-reuse sketch is amended alongside this decision; note
  the allocator must stay within LSP's integer range, −2³¹..2³¹−1 —
  the current unconstrained `i64` is a latent conformance gap that
  wrapping-with-collision-check or string ids closes), but
  registered with no upstream cancellation mapping and with routing's
  non-liveness classification; the driver retains the **exact handle**
  and id for each dispatch, and a registration failure counts as that
  provider's failure (a `null` in the walk). Cleanup on every terminal
  outcome of the decision — early winner, incarnation abort, teardown,
  expiry — cancels and retires **through the retained handle**, never
  by re-resolving the `ConnectionKey`: after a replacement, the key
  maps to a new handle whose restarted allocator can have reissued the
  same id to an unrelated request, so key-resolved cancellation could
  cancel a stranger. Retirement must be atomic with the decision's
  settlement so late answers drop.
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
  point; the flush is O(1) under the lock — the map is *taken* (swapped
  for an empty one) with the epoch bump and cause-ring append, and the
  taken map is dropped outside the critical section, so provider churn
  never stalls pool operations on entry-drop cost.
- The frame reader currently allocates the declared `Content-Length`
  before parsing anything, so the answer-size bound needs a
  **framing-level ceiling on downstream message size** — a transport
  hardening recorded here as an implementation prerequisite of this
  protocol's allocation bound, not a routing-handler check (which
  would run too late). Its disposition is chosen, not deferred: a
  header declaring more than the ceiling is a **framing error and
  fails the downstream connection**, never a drain (draining an
  attacker-sized body can hang the reader) — the same fatal posture
  every downstream framing violation already gets. Header-side
  allocation is bounded the same way: maximum header-line and total
  header-block sizes, enforced incrementally as bytes accumulate. The
  ceilings' values and compatibility consequences are recorded in the
  reader's decision (ls-bridge-async-connection, amended with this
  ADR): generous implementation-defined defaults that trip on runaway
  peers, not big workspaces.
- ls-bridge-timeout-hierarchy gains the routing decision deadline
  (registered beside the per-slot control shutdown timeout), the
  binding-reuse validation budget, and the Tier-2 exclusion note; that
  edit lands with this ADR.

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
| **Providers** | all spawnable configured servers ∩ advertising ∩ `Ready` (Initializing awaited only when the advertisement is known, the server is named, or it carries `forceStart`), ordered by routing `priorities` (no `"_"` method-wildcard inheritance); concurrent fan-out, `preferred` fan-in, operative-entry rule |
| **Deadline** | one routing timeout per decision (low-seconds class, registered in ls-bridge-timeout-hierarchy, plus a separate binding-reuse validation budget); expiry is partial-result (cancel unanswered, drop unfinished entries, fan-in over what normalized; whole fallback only when no operative normalized result remains); exempt from Tier-1 and Tier-2 accounting; awaited in the open tasks, never under the ingress ticket |
| **Caching** | decision cache per (host URI, layer, languageId, config generation), single-flight, evicted on `didClose` and on an injection tuple's authoritative last-region disappearance, flushed on reload / `Ready`-provider-set / workspace-folder-set change, (generation, flush-epoch, open-incarnation)-anchored; applied outcomes live in a per-document **route binding** until `didClose` (injection tuples: also last-region disappearance); never retroactive |
| **Cold start** | `forceStart` (post-config-publication get-or-create; `#shared` + primary-root seed for `preferSharedInstance` servers, the marker-less fallback shape otherwise; warm-up scope limited to shared/marker-less/policy servers; wait-eligible for the initialization wait) + bounded initialization wait inside the decision deadline, woken by any handshake exit |
| **Recursion** | provider connections, queries, and the re-open sweep never trigger routing queries; the sweep reads each exact server entry's binding where one exists, marker resolution where none does, and never spawns |
