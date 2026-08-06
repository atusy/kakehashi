# Workspace Scoped Symbol Search

**Related Decisions**: [cross-layer-aggregation](cross-layer-aggregation.md),
[language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md),
[language-server-bridge-request-strategies](language-server-bridge-request-strategies.md),
[aggregation-priorities-wildcard](aggregation-priorities-wildcard.md),
[host-document-bridge](host-document-bridge.md),
[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md),
[ls-bridge-timeout-hierarchy](ls-bridge-timeout-hierarchy.md),
[respawn-reopen-derives-its-targets](respawn-reopen-derives-its-targets.md),
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md)

## Context

`workspace/symbol` is the first request kakehashi bridges that carries **no
`textDocument`**. Every existing bridged request resolves a host document, then
an injection region, then a `RegionOffset`, and that chain is what makes the
current fan-out and fan-in work:

- **Fan-out** is driven by `bridge_configs_for_injection_language(host_language,
  injection_language)` — the region's language picks the servers.
- **Fan-in** is driven by `transform_location_for_goto(location,
  request_virtual_uri, host_uri, offset)`, whose contract is "translate results
  addressed to *the request's* virtual URI, drop every other virtual URI".
- **Arbitration** is `preferred`, first-non-empty-wins, at both the bridge and
  the layer level.

None of the three survives the loss of the document. There is no injection
language to select servers with, and no single virtual URI to translate against.

The arbitration problem is subtler than "no document", and it is the one that
shapes this decision: a `workspace/symbol` response is **not attributable to a
language**. A server answers "here are the workspace's symbols matching
`query`", and what it indexes is its workspace root, not a language — so a
server reached through one configured **pair** (throughout this decision, a
`(host_language, bridge_key)` entry in `languages.<lang>.bridge.<key>`)
routinely returns symbols belonging to another pair's language. Nothing in the response says which configured pair
"owns" an entry, so no arbitration between pairs can be principled.

```
DOCUMENT-SCOPED  (every existing bridged request)

  params.textDocument.uri
         │
         ▼
   host document ──▶ region at position ──▶ RegionOffset ──▶ one virtual URI
         │                   │                    │                 │
         │                   ▼                    └────────┬────────┘
         │           injection_language                    ▼
         │                   │                    FAN-IN filter: keep THIS
         │                   ▼                    virtual URI, translate by
         │      bridge_configs_for_               THIS offset, drop every
         │      injection_language()              other virtual URI
         │                   │
         │                   ▼
         │              FAN-OUT set
         ▼
   arbitration: first non-empty wins (preferred)


WORKSPACE-SCOPED  (workspace/symbol)

  params.query                        ← no textDocument at all
         │
         ✗ no host document   ✗ no region   ✗ no offset   ✗ no virtual URI
         │
         ├─▶ FAN-OUT cannot be selected by injection language
         ├─▶ FAN-IN has no "request virtual URI" to filter against
         └─▶ ARBITRATION has no way to attribute an entry to a pair, so any
             "prefer A over B" silently discards symbols on a basis kakehashi
             cannot compute
```

Three facts in the existing code make the feature tractable anyway:

- The pool's tracker holds a **global** virtual→host mapping — it is what
  `window/showDocument` translation reaches through `resolve_virtual_uri` — and
  the document store keeps **generation-stamped resolved-region snapshots**,
  from which a region's offset, end, content, and language can be read without
  re-resolving anything.
- `LanguageServerPool::connections()` enumerates the pool's connection map.
- `send_execute_command_on_handle` (`bridge/workspace/execute_command.rs`) is an
  existing precedent for sending a request on a connection **without** a virtual
  document, and it shows exactly what that costs (see point 4).

kakehashi today sends downstream no `workspace.symbol` client capability at all
(`bridge/protocol/client_capabilities.rs` sets no `symbol` field on
`WorkspaceClientCapabilities`), so in particular no `resolveSupport`. Per LSP
3.18, a conformant downstream must therefore return a full `Location` rather
than the location-without-range form. Point 5 keeps that property for
`resolveSupport` while declaring the rest of the capability.

Absence is the right answer only for `resolveSupport`. `symbolKind` and
`tagSupport` are independent of it, and leaving the whole capability absent
tells a conformant server to omit tags and to restrict itself to the legacy
`File`–`Array` kind set — silently degrading results kakehashi's own client
could have used. So this decision **declares `workspace.symbol` downstream**,
mirroring the upstream client's `symbolKind` and `tagSupport` while keeping
`resolveSupport` absent. Mirroring rather than asserting is deliberate:
kakehashi must not accept a kind or tag it cannot pass on.

Exact mirroring has one exception, and it is the legacy spelling again. An
upstream `tagSupport: true` deserializes to an empty `value_set`, and mirroring
that verbatim would advertise `{ valueSet: [] }` downstream — telling a
conformant server that no tag is supported. The legacy `deprecated` field is
modelled independently and is not forbidden by that, but neither is it
guaranteed: a server may reasonably send neither form, leaving the fallback
below nothing to preserve. For that
representation kakehashi advertises `DEPRECATED` downstream instead, because it
can convert a tag back into the legacy boolean but cannot invent information a
server was told not to send.

Nothing currently advertises the provider: `workspace_symbol_provider` is never
set in the initialize result, and `LanguageServer::symbol` is never overridden,
so the request today reaches tower-lsp-server's default and is unimplemented.

## Decision

Implement `workspace/symbol` as a **workspace-scoped** bridged request: a
candidate-set walk over configuration intersected with live connections, a
per-entry global fan-in translator, and a single deduplicating union. Defer
`workspaceSymbol/resolve`.

```
 client                    kakehashi                    downstream servers
   │
   │ workspace/symbol   ┌───────────────┐
   │  { query }    ────▶│ fan-out (§3): │─── request ──▶  lua_ls   @ rootA
   │                    │ pairs ∩ live, │─── request ──▶  tsgo     @ rootA
   │                    │ capped, then  │─── request ──▶  tsgo     @ rootB
   │                    │ deduped       │
   │                    └───────────────┘                         │
   │                                                              │
   │                    ┌───────────────┐                         │
   │                    │ classify and  │◀── every response ──────┘
   │                    │ translate     │    NOT first-win: every target
   │                    │ (fan-in, §5)  │    is awaited before answering
   │                    └───────┬───────┘
   │                            ▼
   │             ┌────────────────────────────────┐
   │             │ union (§1) — the ONLY merge,   │
   │             │ no per-target arbitration      │
   │             │  1. collect every entry        │
   │             │  2. dedup  (name, kind,        │
   │             │             uri, range)        │
   │             │  3. sort   (uri, start, end,   │
   │             │             name, kind)        │
   │             └───────────────┬────────────────┘
   │◀── Symbol array ────────────┘
   │    (element type per client, §5; never null)
   │
   │ $/cancelRequest ─▶ forwarded to every in-flight target, best effort:
   │                    a downstream may ignore it (LSP allows this). The
   │                    hard bounds are the pool's own timeouts (§6).
```

### 1. A dedicated `union` aggregation strategy

`AggregationStrategy` gains a third variant:

```rust
pub enum AggregationStrategy {
    Preferred,
    Concatenated,
    Union,
}
```

`Union` = collect from **every** target, then **deduplicate** on
`(name, kind, uri, range)`, then **sort deterministically** by
`(uri, start.line, start.character, end.line, end.character, name, kind)`.

The sort key is deliberately a **superset** of the dedup key. Two entries that
survive dedup differ in at least one key field, so a superset sort leaves no
ties and the order cannot fall back to `JoinSet` completion order. Dropping
`end` from the sort would break exactly the case this decision predicts below —
two servers agreeing on a symbol's start and disagreeing on its end.

It is a distinct value rather than a reuse of `Concatenated` because
`Concatenated` deliberately preserves duplicates and source ordering (diagnostics
and code actions from different servers are complementary and each must survive).
A symbol search is set-valued: the same real file indexed by two servers, or by
one server under two workspace roots, should appear once. Sorting is part of the
strategy, not a caller detail — `JoinSet` completion order is nondeterministic
and an unstable result order is a self-inflicted test flake.

`container_name` is deliberately **excluded** from the dedup key. LSP defines it
as "for user interface purposes… It can't be used to re-infer a hierarchy" and
imposes no format or stability contract, so two servers naming the same symbol
`"Foo"` and `"Foo.Bar"` (or omitting it) would defeat dedup on a field the spec
says carries no identity. `(uri, range)` after translation already identifies a
location; `(name, kind)` guards against a server reporting several symbols at one
range.

Dedup is **exact-match only**. Two servers that disagree on range granularity —
a full-declaration span versus a name-only span — produce two entries that both
survive. That is a real limit of the mechanism, not something the strategy
promises away.

When two entries *do* collide, the survivor's non-key fields are **merged by
rule**, not taken from whichever arrived first — otherwise the payload would
still vary with `JoinSet` completion order even though its ordering does not.
`tags` are unioned and sorted; `container_name` takes the lexicographically
smallest `Some`, or `None` if no entry carried one; `data` is **dropped
entirely**, since it is server-private state whose only purpose is
`workspaceSymbol/resolve`, which this decision does not support.

`Union` is **normalized away at resolution time** for every other method:
`resolve_aggregation` yields `Union` only for `workspace/symbol`, and a `Union`
configured anywhere else resolves to that method's existing default before any
handler sees it. The same normalization applies to `LayerAggregationConfig`,
which shares this enum.

Normalizing at the single resolution point — rather than adding behavior at each
consumer — is not a style preference. `AggregationStrategy` is matched
exhaustively at 7 sites, but `plan_region_format` does **not** match: it tests
`strategy != Concatenated` and treats everything else as `Preferred`. So a
`Union` that reached the handlers would mean `Concatenated` at the exhaustive
sites and `Preferred` inside formatting — one configured value with two
behaviors in the same method. And because a method-level `"_"` entry resolves
into every method, `_ = union` is a configuration a user can plausibly write,
so this is reachable, not hypothetical.

The exhaustive sites still need a `Union` arm to compile; with normalization in
front of them it is unreachable, and each site should say so rather than invent
a behavior. `Union` is meaningful for `workspace/symbol` and nothing else.

`workspace/symbol` does not go through the cross-layer walk at all — there are
no layers without a document — so only the bridge-level default matters.
The cross-layer-aggregation per-method table gains no row.

### 2. `preferred` is wrong for this method, and is overridden

The repo's own criterion for choosing a strategy is stated at
`default_aggregation_strategy_for_method`: code actions concatenate because,
"unlike formatting (**competing** whole-document edits), code actions from
different servers are **complementary**."

Two servers' `workspace/symbol` responses are complementary in that sense, and —
per the attribution argument in Context — kakehashi cannot even tell which parts
of a response a given pair "asked for". Concretely, given

```toml
[languages.LANG_1.bridge._self.aggregation]
workspace/symbol = { priorities = ["B"] }

[languages.LANG_2.bridge._self.aggregation]
workspace/symbol = { priorities = ["C"] }
```

B's LANG_2-related symbols are **kept**. Dropping them because LANG_2 nominates
C would assume C covers what B indexes — unknowable, and pure loss whenever it
is false.

An explicit non-`union` strategy for this method is therefore overridden to
`union`, with one warning at settings-apply time, following the existing
misconfiguration path (`misconfigured_settings_warnings`, which already warns
about `concatenated`-without-`priorities` for formatting). The same walk warns
in the **other** direction too: `strategy = "union"` on any method other than
`workspace/symbol` is equally inert (point 1), and warning on only one of the
two would leave the more likely user mistake silent.

The override and the warning ask **different questions**, and conflating them
would make the warning fire for everyone.

The **override** asks "what strategy would this method resolve to?", so it
resolves through `resolve_with_wildcard` like everything else — a method-level
`"_"` entry legitimately supplies a strategy to `workspace/symbol`, and the
override must catch that.

The **warning** asks "did a user ask for this?", and must not answer it by
resolution. The built-in defaults themselves install a wildcard `preferred`, so
after resolution an untouched session is indistinguishable from a hand-written
override — a warning driven off the resolved value would fire for every user.
Nor can it be answered from provenance: `load_settings` merges the default,
user, project and override layers and keeps only the merged result, so no layer
attribution survives, and naively scanning the original layers would warn about
a lower-precedence `preferred` that a higher layer already replaced with
`union`.

So the warning fires only on a **method-specific `workspace/symbol` entry**
whose strategy is non-`union`. Writing that key is unambiguously deliberate, the
shipped defaults never do it, and it needs no provenance machinery. A `_`
wildcard that happens to reach this method is silently overridden without a
warning — deliberately, since it is far more likely to be the user configuring
everything else than to be an opinion about workspace symbols.

Two servers indexing the *identical* root — say two TypeScript servers — are
genuinely competing rather than complementary, and this decision does make their
near-duplicate entries survive when their ranges differ. The lever for that case
is `priorities`, which is an allowlist: name only the server you want. That is a
static, deliberate exclusion the user writes, not an inference kakehashi draws
from an empty response — which is the distinction that makes it acceptable here
and `preferred` not. It does cost the user an edit in **every** pair that names
the unwanted server, because fan-out unions candidates across all pairs.

### 3. Fan-out: select synchronously, and never wait to select

A request has no document, so it cannot pick one `(host_language, bridge_key)`
pair — it walks **all** of them, purely to collect candidates. Both axes must be
**concrete**, and neither is the config map's raw key set.

`"_"` is a configuration **template**, never a target. Everywhere else in
kakehashi it is resolved *against an actual language*:
`is_language_bridgeable("python")` merges `_` into `python`, and servers are
then selected by whether they handle `python`. Walking `bridge_map.keys()`
literally would ask for the servers that handle a language named `_` — and
because the shipped defaults populate only `languages._.bridge._`, the default
configuration would contribute **no candidates at all**. Treating `_` instead as
"every server" is worse: it would re-admit servers that a concrete
`bridge.python.enabled = false` or `priorities = []` excluded, and union has no
way to subtract that contribution.

Both axes therefore come from **what is actually open**, which is also the set
the live-only rule already commits to:

- **Host languages**: the languages of the currently open documents
  (`DocumentStore::open_uris()` + language detection). Concrete by
  construction — it never comes from a map key.
- **Injection languages**: the languages of the currently **open virtual
  documents**, read from `VirtualDocumentUri::language()` on the pool's tracked
  `OpenedVirtualDoc` entries, plus `_self` for the host tier.

Deriving the injection axis from configuration instead — concrete bridge keys
plus the languages servers declare — looks equivalent and is not. It loses every
`languages = ["*"]` server, which is the whole point of the any-language-server
wildcard: an open Markdown document with Python fences, the default `bridge._`,
and only a `["*"]` server configured yields no non-`_` bridge key and no
declared concrete language, so no injection language would be generated and the
server would never be asked. Normal routing avoids this because it starts from
the region's actual `"python"` and lets `handles_language` recognize `"*"`.
Starting from open virtual documents restores exactly that: the language is the
region's real one.

`OpenedVirtualDoc` also carries the `connection_key` the document was opened on,
so the walk can pair each open virtual document with its host document's
language directly rather than taking a cross product of the two axes.

An entry must be **validated before it contributes**, because the tracker's
host→virtual map is not an "already open downstream" set: `register_pending_document`
inserts an entry to own its close-cleanup *before* `didOpen` reaches the writer
FIFO, and only `mark_open_sent` promotes it into the live reverse index. A
cloned entry can also outlive a connection purge. Taken at face value, the walk
would derive a pair from a `didOpen` the downstream has not seen, or from a
replaced connection whose documents were never replayed.

The save path already establishes the discipline this needs, and the reasoning
there applies unchanged: snapshot the tracker, then take `connections` **once**
and hold it across the checks with no `.await` inside, require the handle to be
the current `Ready` one, and only then consult
`is_virtual_doc_open_on_connection`. Holding `connections` is what makes it
sound — a reverse-index check alone would not, since a purge could swap in a
fresh `Ready` handle that never opened the document. The lock order is
`connections` → tracker, matching the respawn purge. This composes with the
stale-handle re-check the send already performs (point 4): the enqueue is
non-blocking, so both happen under the same lock and only the response is
awaited outside it.

Candidates for each `(host, injection)` pair then come from the **existing
routing entry point**, `get_all_configs_for_language`, rather than from a
hand-rolled lookup. That function already applies the `is_language_bridgeable`
gate — with `_` inheritance — and already selects servers by whether they handle
the injection language, so reusing it is what keeps this method's routing
identical to every other method's. `_self` goes through the host tier's own
gate instead (`is_host_bridging_enabled`), which is opt-in and direct, unlike
the opt-out, wildcard-resolved gate beside it.

`priorities` / `strategy` / `max_fan_out` are read with
`resolve_aggregation(method)` on the wildcard-resolved bridge config, so values
set only on `bridge._` still apply to a concrete pair — the property that keeps
existing configuration from becoming a silent no-op here.

**Selection is entirely synchronous. No candidate is waited for:**

1. Take the pool's connection map and keep only `Ready` entries.
   `connections()` returns the raw map, and `ConnectionState` also has
   `Initializing`, `Failed`, `Closing`, and `Closed`.
2. Drop handles that lack `workspace/symbol`. This is knowable now, because
   every candidate is `Ready` — `server_capabilities()` is populated during the
   handshake.
3. Apply the per-pair `priorities` allowlist and `max_fan_out` cap.
4. Dedup the survivors by **connection key** `(server, root)`.
5. Await `wait_for_pending_reopen` for the survivors, **concurrently**, then
   send.

Step 5 exists because a connection reaches `Ready` *before* its virtual
documents are replayed after a respawn, so a query landing in that window would
under-report that server's embedded symbols indistinguishably from "no matches".
The barrier is **bounded to two seconds** and enforced with a timeout, and
awaiting the survivors concurrently costs that bound once for the whole query —
not once per target.

A target whose barrier **fails** — timeout, or repair that did not complete — is
**dropped, not sent to**. The barrier reports that outcome and its contract is
that callers must not proceed; sending anyway would query a connection known to
be missing its documents, which is the failure step 5 exists to prevent. Losing
that one target is the same coverage cost point 7 already describes, now with a
bound on how long the query waits to find out.

Selection's outer deadline (point 6) must exceed this two-second budget, or it
would cancel the barrier it is waiting for; **five seconds** leaves room for the
`connections` acquisition ahead of it without letting an unrelated stall hold
the query indefinitely.

**`Initializing` connections are excluded.** Including them, and waiting for
Ready before dispatch, was tried and abandoned — it looked like it removed a
timing dependence and instead produced a cascade:

- Capability is unknowable for an `Initializing` handle, so the cap either ran
  before the capability check (letting a known-incapable server take the last
  slot and answer nobody) or after the wait (making the cap a global barrier
  that delayed a ready target by another target's 30-second `wait_for_ready`).
- A reserved slot could die four ways — timeout, failure, close, handle
  replacement — turning `max_fan_out = 1` with a slow high-priority candidate
  from "slower" into "empty".
- Backfilling the dead slot has no well-defined unit: the cap selects server
  *names* per pair, while failure happens per `(server, root)` *connection*, and
  a deduplicated connection can carry several pairs' priority lists, so there is
  no single "next candidate". Sequential backfill also gives each replacement a
  fresh 30-second wait, so three slow candidates cost 90 seconds before any
  response wait — breaking the latency account outright.

Excluding them costs one thing: a query issued in the seconds after opening a
file may miss a server still starting for it. That is not a new class of
surprise — this method's contract is already "coverage is what is live", and
coverage already grows and shrinks under the client's own actions (point 7). It
buys a selection that is synchronous, deterministic, capability-exact, and free
of every failure mode above.

The cap sits after the liveness and capability filters, and that placement is
load-bearing.

Putting the cap *before* the liveness filter would let dead or absent servers
consume cap slots and silently exclude a live server ranked below the cutoff,
contradicting this decision's own coverage contract. (In the abandoned waiting
design this placement had a second failure mode too — the cap became a global
barrier — but with `Initializing` excluded there is nothing left to wait for,
so only the coverage argument applies.)
Deciding membership synchronously removes the interaction entirely.

Step 2 must precede step 3: capping first would let a **known-incapable**
server take a slot from a capable one — with `priorities = [A, B]`,
`max_fan_out = 1`, and A incapable, the query would consult nobody. Keeping only
`Ready` candidates is what makes that ordering possible at all, since capability
is unknowable before the handshake.

`priorities` is an allowlist: listed servers are candidates, `"*"` stands for
the rest, and an explicit `[]` remains the per-method kill switch. Its **order**
still decides which N survive a `max_fan_out` cap (`truncate_entries` keeps the
highest-priority N in walk order); only the *arbitration* meaning of order is
dropped. A `_self` pair contributes candidates only when
`bridge._self.enabled = true` for that language — `is_host_bridging_enabled` is
a **direct** lookup with no wildcard fallback, unlike the aggregation fields
beside it, so a `bridge._self.aggregation` block without `enabled` contributes
nothing at all.

Every **other** bridge key is gated by `is_language_bridgeable`, which resolves
`enabled` through the wildcard and defaults to `true` — and which
`get_all_configs_for_language` already applies, which is the main reason to
route through it rather than re-deriving the gates.

Dedup is by connection key, not server name: the response depends on the
connection's own indexed workspace, so a connection named by several pairs is
asked **once**, while the same server name under two roots is genuinely two
connections and two requests.

`max_fan_out` is applied **twice**, and the second application is what makes it
mean what it says.

Within a pair it works as everywhere else: `truncate_entries` truncates a
flattened name list in walk order, keeping the highest-priority N names. A
surviving name contributes every connection that survived steps 1 and 2 — every
`Ready`, capable handle in the selection snapshot — so if `A/root1` advertises
`workspace/symbol` and `A/root2` does not, selecting the name A must not smuggle
root2 back past the capability filter.

But a per-pair name cap bounds neither requests nor connections here: one name
can mean several roots, and a connection one pair's cap excluded re-enters
through another pair that names it. Left there, `max_fan_out = 1` could produce
arbitrarily many requests, contradicting the setting's documented promise to
"cap the number of concurrent server requests" — a generic load control quietly
losing its load-controlling property in exactly the method most able to fan out.

A second, global cap after dedup was tried and **rejected**. It cannot be given
coherent semantics:

- An uncapped pair must mean "no limit" — that is what `None` means everywhere
  else — so any workspace containing one uncapped pair has no global cap at all.
  Since uncapped is the default, the cap would be inert in almost every real
  configuration, and present only where a user capped *something*, throttling
  languages they never mentioned.
- It has no principled victim order. `priorities` exists per pair and pairs may
  rank the same server differently; merging those rankings is exactly what this
  decision rejects elsewhere as unprincipled. Falling back to walk order is
  worse than arbitrary — open documents and connections live in hash maps, so
  which connections survived would vary run to run.

So `max_fan_out` stays a **per-pair name-selection rule** here and bounds
neither the query's requests nor its connections. That is a real divergence from
its documented promise to "cap the number of concurrent server requests", and
the fix is documentation, not a mechanism: the setting's description must say so
(point 8). What actually bounds the total is the live-connection set (point 7).
A workspace-level ceiling, if one is ever wanted, belongs in a separate setting
with its own name and semantics rather than smuggled into this one.

```
  open host docs × their open virtual docs    ← concrete languages only,
        │  (never a raw `_` map key)             no arbitration
        ▼
  ┌──────────────────────┐   ┌──────────────────────┐
  │ (LANG_1, _self)      │   │ (LANG_2, _self)      │  ... every pair
  │ priorities ["B"]     │   │ priorities ["C"]     │
  └──────────┬───────────┘   └──────────┬───────────┘
             └────────────┬─────────────┘
                          ▼
        ┌──────────────────────────────────────┐
        │ SELECTION (own outer deadline, §6)   │
        │ 1. keep Ready ONLY (not Initializing,│
        │    Failed, Closing, Closed)          │
        │ 2. drop handles lacking capability   │
        │ 3. allowlist + per-pair max_fan_out  │
        │    NEVER spawns a connection         │
        └──────────────────┬───────────────────┘
                           ▼
        ┌──────────────────────────────────────┐
        │ dedup by CONNECTION KEY (server,root)│
        │   (B, rootA)  ← named by LANG_1      │
        │   (B, rootB)  ← same server, another │
        │                 root: 2 connections  │
        │   (C, rootA)  ← named by LANG_2      │
        │ (no global cap — see §3)             │
        └──────────────────┬───────────────────┘
                           ▼
              await the 2s reopen barrier concurrently,
              dropping targets whose repair failed,
              then send to each survivor (§4),
              then per-entry fan-in (§5),
              then UNION → dedup → sort (§1)
```

### 4. The send lives in the `bridge` module; the translation does not

`connections()` is `pub(super)` to `crate::lsp::bridge`, and `lsp_impl` is a
sibling module, not a descendant — so the fan-out **cannot** live beside the
handler. It goes in `src/lsp/bridge/workspace/symbol.rs` as an
`impl LanguageServerPool`, exactly where `execute_command.rs` already puts the
one existing document-free send.

That precedent also shows the primitive list is longer than
`has_capability` / `send_request` / `wait_for_response`. Per target:
`register_upstream_request(id, key)` at the pool level, then
`handle.register_request_with_upstream(...)`, a `RouterCleanupGuard` armed
around the request, and — under the `connections()` lock — a re-check that the
handle is *still* the pool's current connection for its key before enqueueing,
because a concurrent respawn may have replaced it. Unregistering on every early
return is part of the pattern.

`has_capability` needs two things before it can be used at all:

- **A `workspace/symbol` arm.** Its `match` ends in `_ => false`, so an unlisted
  method reports every server incapable. The arm reads
  `workspace_symbol_provider: Option<OneOf<bool, WorkspaceSymbolOptions>>` in the
  same shape as the existing `textDocument/definition` arm.
- **A `Ready` handle to read.** It falls back to
  `server_capabilities()`, which is `None` until `set_server_capabilities` runs
  during the handshake. Point 3 keeps only `Ready` candidates precisely so this
  is knowable at selection time — an `Initializing` handle reports every server
  incapable, which is why it cannot be a candidate.

Cancellation needs nothing new **downstream**: `register_upstream_request`
already holds many `(server, root)` keys per upstream id,
`forward_cancel_by_upstream_id_if_current` already iterates all of them, and
`UpstreamRegistrySweepGuard` unregisters the whole entry. Multi-target
cancellation falls out of using the pattern.

It does need something **upstream**. Downstream registration happens inside the
per-target send, so it can only cancel work that has already been dispatched —
it does nothing for the selection and index-building the handler does first, nor
for the handler's own future. The handler therefore subscribes to
`$/cancelRequest` **before its first await** and selects against the whole
dispatch, exactly as the existing document-free handler does. Not because the
signal would otherwise be lost — the request registry latches a cancel that
arrives between request acceptance and subscription and delivers it on
subscribe — but because selecting on it is what makes the handler abandon
promptly rather than only at its next dispatch boundary.

Fan-in lives in `lsp_impl`. Not because it must — the store's snapshot
accessor is `pub(crate)`, so a `bridge` module handed the store could call it —
but because that is where the `DocumentStore` / `LanguageCoordinator` /
`BridgeCoordinator` handles already sit together, as they do for
`ShowDocumentTranslator`. Putting it in `bridge` would mean threading the store
into a module whose job is wire protocol. This is a coupling choice, unlike the
fan-out's placement in point 4, which is a real visibility constraint.

The value crossing that boundary is **typed, not raw JSON**: the bridge module
owns deserialization for every other bridged request, and this one keeps that
property. Each target's response is parsed into `Vec<WorkspaceSymbol>` there,
normalizing a `SymbolInformation[]` answer into the same shape. That works for
`location` because `WorkspaceSymbol.location` is a `OneOf` that models the
range-less form rather than rejecting it, but it is not field-for-field:
`SymbolInformation` carries a (itself deprecated) `deprecated: Option<bool>`
that `WorkspaceSymbol` has no counterpart for, so the normalization must fold
`Some(true)` into `tags` as `SymbolTag::DEPRECATED` or silently lose it.

`lsp_impl` then classifies and translates those typed values. Its classification
is unit-testable as a pure function once the URI index and the per-host geometry
map are injected — both are plain data, which is a side benefit of fan-in
resolving nothing.

### 5. Fan-in: a global virtual→host translator

Every entry is classified independently, and each is translated against **its
own** region's offset — which is what lets this path cross blocks where the goto
path may not: the goto filter exists because only one region's offset is in
hand. "Its own offset" does not mean "its own resolution", though; see pass 3.

```
  one entry of the downstream's result array
             │
             ▼
     ┌───────────────────┐  no
     │ has a range?      ├─────▶ DROP  (uri-only location — see below)
     └─────────┬─────────┘
               │ yes
               ▼
     ┌───────────────────┐  no
     │ is_virtual_uri ?  ├─────▶ REAL FILE ─▶ pass through untouched
     └─────────┬─────────┘
               │ yes
               ▼
     ┌───────────────────┐  yes
     │ is_scratch_uri ?  ├─────▶ DROP  (a formatting scratch document; names
     └─────────┬─────────┘             no place in any host file)
               │ no
               ▼
     look up in the REQUEST-LOCAL URI index (pass 0)
               │
               ├── absent ───────▶ DROP  (retired region, or a real file in
               │                          the reserved virtual-URI namespace)
               ▼ (host_url, region_id, language)
     look up region_id in that HOST's geometry map (pass 3, read once)
               │
               ├── absent ───────▶ DROP  (region invalidated by edits, or
               │                          host document closed)
               ├── language ≠ ────▶ DROP  (region now hosts another language)
               │   current
               ▼ (offset, region_end, virtual_content)
     TRANSLATE
       uri   := host_url
       range := translate_virtual_range_to_host(range, offset)
       └─ validate against virtual_content and region_end — see below

  Neither lookup rescans: the URI index is built once per request and the
  region map once per host.
```

Bounds validation is necessary but **not sufficient**, so translation also
pins the content the result was computed from. A range measured against an older
version of a region can remain perfectly in-bounds in the current one — insert a
line above a symbol while the request is in flight and its old range still
validates, then translates onto unrelated text. No amount of geometry checking
catches that, because the geometry is fine; what changed is the text the
downstream was looking at.

Each virtual document's **content identity at dispatch** is therefore captured
and re-checked before its entries are translated, using the per-connection
revision and the fingerprint of the content last *confirmed sent* that the
tracker already maintains.

Comparing those two values across dispatch and translation is not enough on its
own. The revision advances before the `didChange` is enqueued and stays advanced
when the enqueue fails, while the confirmed-sent fingerprint deliberately does
not move — so after an `A → B` edit whose notification was dropped, both
readings agree at `(revision 2, fingerprint A)` while the current geometry
describes B and the server is still answering about A. Stability across the
request proves nothing when both halves were already wrong.

The confirmed-sent fingerprint must therefore also **equal the current region's
content identity**. That is the check that ties what the server saw to what the
geometry describes; the dispatch/translation comparison only catches movement
during the request. Entries failing either are dropped, exactly like entries
whose region moved.

With that in place, translation **validates the region bounds** too; it does not
translate blindly.
Region ids deliberately survive edits, so an in-flight response can carry a
range measured against an older, larger region while offset resolution against
the current parse still succeeds — and plain range translation performs no
boundary check, so the result could point into the closing fence or the host
text after it. The region map already carries each region's current
`region_end` alongside its offset, but that bound alone is not enough: a stale
virtual position like `(0, 1000)` inside a region ending on line 5 compares as
before the end while carrying a column that never existed, and the workspace-edit
precedent checks only per-line floors and a global endpoint.

Validation therefore runs against the region's **current `virtual_content`** —
the same field the language check above needs retained: both endpoints must be
real positions in that text and the range must be correctly ordered, using the
existing strict position machinery rather than a new comparison, and only then
is the range translated and checked against the host-side region bound. Validating in virtual coordinates before translating is
what catches the column case; the host bound catches what survives it.

The range check comes first because `WorkspaceSymbol.location` is
`OneOf<Location, WorkspaceLocation>` and the `WorkspaceLocation` form carries a
`uri` and nothing else. A conformant downstream cannot send it here — kakehashi
declares no `resolveSupport` (point 9) — but `OneOf` is `#[serde(untagged)]`, so
it deserializes anyway. The classifier must reject it explicitly rather than
reach for a `range` that is not there.

Dropping — rather than passing through — an unresolvable virtual URI is
deliberate: a virtual URI that escapes to the editor names a file that does not
exist on disk, so the symbol is unopenable. This mirrors `window/showDocument`
translation, which drops the selection it cannot translate.

**Parse freshness is load-bearing here.** The region snapshot tracks the live
parse, and `didChange` clears the tree and reparses off-ingress, so during the
reparse window the edited document has no current geometry for any of its
regions — which
this classifier would silently turn into "no embedded symbols". The
whole-document handlers avoid this by calling `ensure_document_parsed` first;
this method has no target document to name.

Fan-in therefore runs in **four passes**, not one, over a **request-local
index** built once up front:

0. **After every target's result is collected**, and not before, snapshot the
   tracker's host→virtual map once and build a
   `virtual_uri_string → (host_url, region_id, language)` map for this request.
1. Classify every entry against that index, **grouping entries by host**. No
   parse and no lock is involved, so nothing waits here.
2. Ensure the distinct hosts that actually appear, **concurrently**.
3. Read each host's region geometry **once** from its snapshot, then translate
   that host's entries against it.

Pass 0 is built **late, and verified late**, because a single early snapshot
is wrong in both directions across a collection phase that runs concurrently and
can reach the 30-second response bound.

Too-early **under-reports**: a virtual document opened after the snapshot but
before a downstream request can legitimately appear in that response, and would
then be dropped for being absent from a map that predates it. Waiting for the
*first* response is not enough either — other targets can stay in flight for
another 30 seconds and answer with documents opened in the meantime. The index
is therefore built once all results are in hand, immediately before
classification.

Staleness in the other direction is worse and survives any snapshot time, so it
is closed by a check rather than by timing. A region keeps its ULID across
edits, but its **injection language can change** — the close path removes the old
virtual URI and a new one is opened for the new language. The geometry is keyed
by host and region id alone, so a stale entry naming the *old* URI would still
find a live region, and a Python result would be
translated into a region that is now Rust. So the index must carry each entry's
**language**, taken from `OpenedVirtualDoc.virtual_uri.language()`, and that
language must equal the region's current `injection_language`; a mismatch is a
retired document and the entry is dropped.

Comparing *reconstructed URIs* instead would not work. A virtual URI renders the
language only as a file extension, and that mapping is not injective —
`python` and a literal `py` both render `.py`, as do `rust`/`rs` and
`javascript`/`js` — so exactly the language changes most likely to occur would
compare equal. The tracked language string is the identity; the URI is not.

Both that check and the range validation below need data the current helper
throws away rather than data the system lacks: `ResolvedInjection` already
carries `injection_language` and `virtual_content`, and
`resolved_region_geometry` discards both, returning only offset, region end, and
contiguity. The fan-in path needs a snapshot accessor that **retains** them.
No parse and no resolution is involved: fan-in only reads what the store has
already resolved.

Pass 0 is also not an optimization. `BridgeCoordinator::resolve_virtual_uri` is
**not** a map lookup: its own doc comment records that it is "O(N) over open
virtual docs" — the virtual URI encodes the host *directory* and region id but
not the host filename, so the host cannot be derived without a scan — and
justifies that cost with "`window/showDocument` is rare, so the scan is
acceptable". Calling it once per returned symbol destroys exactly that premise:
an interactive endpoint returning thousands of symbols would pay
`symbols × open_virtual_docs`, serialized through repeated acquisition of the
tracker's async mutex. Snapshotting once makes it one scan plus O(1) per entry.
It is a *separate* snapshot from the one point 3 takes to validate candidates —
that one is taken at selection time under the `connections` lock and answers a
different question.

The index also becomes the **identity test**. `is_virtual_uri` is only a
basename pattern — it accepts any URI ending in
`kakehashi-virtual-uri-<id>.<ext>` — so pattern-matching alone would let a real
file that happens to be named that way be treated as virtual. Membership in the
index is the real answer for the entries that matter. What the pattern still
decides is the *drop* case: a pattern match that is absent from the index is
either a region that has since died or a real file with that name, and the two
are indistinguishable. Dropping is chosen, because letting a dead region's
virtual URI reach the editor is the worse failure. **The
`kakehashi-virtual-uri-*` filename space is reserved**; a real file named into
it is not visible to workspace symbol search.

Ensuring one host at a time while translating would make the cost additive:
`distinct_hosts × 200ms` in series, which for a query touching many stale hosts
would dwarf the fan-out it follows and contradict point 6's max-over-targets
account. Grouping first makes the whole pass cost about one 200ms wait.

Sweeping every open document up front instead — alongside the fan-out — was the
first shape considered and is worse on both axes. It does work for documents no
result mentions, and it completes up to 30 seconds before the value is used (a
target may take the full response timeout), so an edit arriving in between
re-clears the tree and the sweep guarantees nothing. Ensuring immediately before the
snapshot is read closes that gap to the width of one pass.

Only documents that **already have a snapshot** are ensured. `ensure_document_parsed`
asks for a 200ms wait, but `wait_for_current_snapshot` escalates to the
15-second `FIRST_PARSE_BACKSTOP` when no snapshot exists at all, regardless of
the caller's wait. Skipping them loses nothing: a never-parsed document has no
resolved injection regions, hence no virtual documents downstream, hence no
result can address it.

The precheck alone does not make the 200ms hold: a close/reopen between the
check and the ensure moves the URI into a fresh, snapshot-less lifetime and the
15-second deadline applies after all. Each ensure therefore carries its **own
outer 200ms timeout**, so the phase's bound is a property of this call site
rather than an inference about the callee's internal state.

**Geometry is read per host, not resolved per entry.** Calling the resolver per
entry would run the whole injection walk over the host every time —
`symbols × regions` work, a 2,000-symbol response walking a 100-region host
2,000 times — while holding a lock that blocks every `didChange` and close for
it. Memoizing individual ids does not fix that: the first lookup for each
distinct region still walks, so the worst case stays quadratic.

Each host's geometry is therefore read **once** into a request-local
`region_id → (offset, region_end, virtual_content, injection_language)` map,
built from the **generation-stamped resolved-region snapshot** the document
store already keeps. When a host has no current snapshot, its entries are
**dropped** — fan-in never resolves inline.

That refusal is the load-bearing part, not a shortcut. Resolving inline would
mean calling `resolve_by_region_id`, which **mutates** the tracker: it reaches
the named-layer allocator and then `calculate_region_id`, which can mint a ULID.
Every attempt to make that safe produced a worse problem than it solved:

- Guarding it with the edit lock and a generation check does not help on
  cancellation. The walk must run on the compute pool (it is documented as
  taking hundreds of milliseconds and having starved Tokio before), and the pool
  **detaches** its work behind a oneshot — dropping the awaiting future does not
  stop the closure. A cancelled query would therefore release both guards while
  `resolve_all` kept mutating the tracker, reopening exactly the ghost-id and
  mixed-generation races the guards existed to close.
- The pool has no queue or execution deadline, so the wait is unbounded — while
  holding that host's edit lock *and* the process-wide settings-reload guard.
  One symbol search could stall edits for a document and configuration reload
  for the whole server.
- Timing out the awaiter does not rescue it; that is precisely the detached-work
  case above.

Dropping keeps the whole fan-in **read-only** — no minting, no detached work, no
unbounded wait, and no global guard held across one — which is what makes every
other guarantee in this section cheap enough to hold.

Its cost is **not** uniformly transient, and this decision does not claim
otherwise. Usually it is: the pre-warm in pass 2 exists to make the snapshot
current, and a host caught mid-reparse is served on the next query. But the
region cache can also be left **persistently** empty. A populate pass refused
after an epoch race publishes a current snapshot whose resolved regions are
`None` and marks parsing finished, while injection processing falls back inline
and still opens the virtual documents — and `ensure_document_parsed` only checks
that the snapshot is current, so it will not repopulate that field. A host in
that state has indexable virtual URIs and no geometry, so symbol search silently
drops it until an unrelated edit forces a reparse.

That is real coverage loss with no signal, and it is accepted here only because
the alternative was the resolve-inline path rejected above. The durable fixes
are outside this decision: populate the region cache whenever virtual documents
exist for a host, or give the cache a repair path an idle reader may trigger
safely. Either is a better place to spend the effort than making fan-in mutate.

Two identities must hold, not one:

- **Document freshness.** The retained content/parsed version *and* incarnation
  are compared against a single live `SnapshotView`. Incarnation alone is
  insufficient: an ordinary edit preserves it and only bumps the content
  version, which `DocumentSnapshot` does not carry. This is the discipline the
  semantic token path already uses, and only the whole of it works — the
  incarnation half alone would leave "the tree is gone" as the only edit race
  the design notices.
- **Query generation.** A settings reload replaces the injection queries and
  bumps the settings generation *before* invalidating parses, and takes no
  document edit lock — so a document-version check alone would accept geometry
  produced under queries that no longer apply. The generation is captured and
  re-checked, as the other query-sensitive paths already do. Because the
  snapshot is generation-stamped, this is a comparison rather than a repair:
  a mismatch drops the entries, and nothing has been mutated to undo.

Translation takes the host's document edit lock before the final freshness
comparison and holds it through translation. The cached snapshot is not exempt
from needing this: it validates freshness only while handing out its `Arc`, so a
`didChange` landing afterwards invalidates the geometry before the translation
uses it, and an unlocked post-check races the same way. The lock is what makes
"validated" and "used" the same instant.

Because the path is read-only, that lock is held only across a map lookup, a
version comparison, and arithmetic — no parse, no walk, no await on another
runtime. That is the whole reason it is safe to hold at all.

Keeping it that short requires care with the range validation above, which is
where the cost hides. The strict position machinery builds a `PositionMapper`
whose line index scans the whole text, and the codebase documents that as
O(document). Rebuilding it per symbol under the lock would be
`symbols × virtual_content` — the same multiplicative shape this section already
rejected once. One mapper is built **per referenced region**, before the lock is
taken; only the freshness comparison and the arithmetic happen inside it.

Taking that lock carries an obligation the happy path hides. `edit_lock`
**creates** an entry unconditionally, so a host that closed between indexing and
fan-in yields no live `SnapshotView` — and simply dropping the symbol would
leave the lock entry behind forever. The miss path must call
`remove_edit_lock_if_unshared`, exactly as the semantic token path does. Fan-in
reaches this case routinely, because the index is built from documents that may
close while other targets are still answering.

A residual race remains and is **accepted**: a `didChange` landing between the
ensure and the freshness check invalidates the geometry, and those entries are
dropped like any other unresolvable ones. Closing it entirely would mean pinning
a per-document snapshot across the whole fan-in and translating against the
pinned text — which would answer with coordinates into text the client has
already replaced. Dropping is the safer failure.

The response to the client is always an **array**, never `null`, so "no server
was running", "everything was dropped", and "every target errored or timed out"
are not distinguished — the spec assigns no distinct meaning to `null` here, and
an array keeps the empty case uniform. This last case needs stating because the
existing fan-in outcomes are mapped inconsistently elsewhere (the diagnostics
path turns a total failure into an empty vector, formatting and code actions
into `None`); here it is the empty array, so a downstream outage degrades to
"no matches" rather than to a protocol-level nothing. A target that errors or
hits the 30-second timeout contributes nothing and does not fail the query.

Entries are emitted as `WorkspaceSymbol[]` — `WorkspaceSymbolResponse::Nested`
in `ls-types`, whose variant names are a misnomer: **both** variants are flat
arrays, and the choice is the element type, not hierarchy (there is no nested
form for this method; `containerName` is spec-documented as unusable for
re-inferring one). `SymbolInformation[]` is the deprecated alternative, emitted only in the
compatibility case below.

One client capability *does* govern the payload, and it turns out to govern the
element type too: `workspace.symbol.tagSupport` declares which `SymbolTag`s the
client accepts, and kakehashi already stores the upstream capabilities.

- A client that can represent `SymbolTag::DEPRECATED` gets `WorkspaceSymbol[]`,
  tags filtered to the set it declared.
- Every other client gets `SymbolInformation[]`.

The discriminator is **representability, not presence**. `tagSupport` cannot be
tested for existence: the legacy boolean form `tagSupport: true` deserializes to
`Some(TagSupport { value_set: [] })`, so a client that declares tag support in
the old spelling would pass a presence check, have every tag filtered away
against its empty set, and lose deprecation exactly as if it had declared
nothing. Asking whether `DEPRECATED` survives the filter answers the question
that actually matters.

The second case is why the element type cannot simply be "always the modern
one". Point 4 *creates* a tag during normalization, folding the legacy
`deprecated` flag into `SymbolTag::DEPRECATED` and discarding the original
field. Emitting `WorkspaceSymbol[]` with tags stripped would therefore destroy
deprecation outright for exactly the clients too old to read tags.

Choosing the legacy element type is not by itself enough to undo that, because
the information now lives in a tag. The legacy path must **reverse the
normalization**: set `deprecated = Some(true)` when the normalized tags contain
`DEPRECATED`, and drop tags the client cannot represent. Selecting
`SymbolInformation[]` without that conversion would lose exactly what selecting
it was meant to preserve. It is bounded: the modern type is used everywhere
else.

### 6. Latency is bounded by the pool's timeouts and one deadline of our own

Every target is awaited, so latency is max-over-targets. The bounds are:

- `wait_for_response` wraps each request in a hardcoded **30-second** timeout and
  removes the router entry when it fires.
- The reader's **liveness timeout** can independently fail a connection that has
  gone silent, transitioning it to `Failed`.
- `$/cancelRequest` is forwarded to every in-flight target, but LSP explicitly
  permits a downstream to ignore `$/` notifications, so it is best-effort and
  cannot be the guarantee.

Selection is not quite free, and the earlier claim that it "adds no wait" was
too strong. It must take the pool's `connections` mutex, and other paths hold
that mutex across `.await`s — eager `didOpen` holds it while opening documents,
and respawn and config invalidation hold it across purges and transition-lock
waits. None of those carries a deadline, and cancel forwarding acquires the same
mutex before it can notify a subscribed handler, so early subscription cannot
interrupt a stall behind it.

Selection therefore runs under its own **outer deadline** — five seconds,
chosen in point 3 to sit above the reopen barrier's two-second budget. Expiring
it answers from whatever was already selectable rather than blocking the query
behind unrelated pool work. This is the one deadline the design imposes; every
other bound it relies on already existed.

Beyond that one deadline, nothing new is introduced: every target is already
`Ready` when chosen (point 3), so nothing waits for a handshake before the
request goes out, and the reopen barrier in step 5 is itself bounded to two
seconds. Because no target is ever cold-started (point 7), the practical case is
bounded by servers that are already running and already answering other
requests.

### 7. Coverage is what is live, and a query never cold-starts a server

A candidate with no live connection is **skipped**, not spawned. Coverage is
"the servers that are running because of what the client has opened", and it
grows as the client opens more files — opening a host document spawns its
servers and opens its virtual documents.

This is not merely a cost trade. Cold-starting cannot deliver the coverage it
appears to promise:

- **Embedded-block symbols cannot be reached by spawning at all.** A virtual
  document does not exist on disk, so a downstream learns of it only through
  `didOpen`, and kakehashi opens virtual documents only for host documents the
  *client* has opened. Reaching embedded code in unopened files would mean
  parsing every candidate host file in the workspace and opening every region —
  precisely the unbounded work this method must not do.
- **Real-file symbols would be reached, but at one root only.** A server spawned
  without a document hint resolves to the `ClientFallback` key, so a multi-root
  workspace gets the client root and none of the marker-derived per-root
  connections.
- **Scoping by "does this language occur in the workspace?" would need a
  mechanism that does not exist.** The LSP server is document-driven and holds
  no workspace index; nothing walks the filesystem on the server path. Under the
  live-only rule that question answers itself: a language with no open file has
  no live server and is not queried.

Coverage is not monotonic. `didClose` is forwarded downstream, a respawned
connection starts with nothing open and regains virtual documents only via a
best-effort re-open sweep, and a `Failed` connection is replaced lazily on next
use — so a host document can stay open with its connection dead. Each of those
shrinks coverage with no signal to the user.

### 8. This is the first deliberate cross-block feature

Every other bridged navigation path filters out results addressed to a region
other than the request's own, because a cross-region offset is unsafe when only
one region's offset is known. Workspace symbol search holds every region's
geometry for the hosts it touches, so each entry is translated by its own
region's offset rather than by a single borrowed one.

Shipping this obliges edits in **both** user-facing docs, in three separate
respects each. They are listed here because more than one review round found the
inventory incomplete.

*The no-cross-block rule is stated as a blanket claim and must be amended, not
appended to* — the wrong part is the framing sentence, while both files'
itemized bodies are already correctly scoped to the goto/references/rename
transforms and stay true:

- `docs/language-features.md` — "Bridged features are also limited to embedded
  code blocks in one respect: navigation and edits do not cross between blocks",
  and separately "features that need to see across blocks do not work between
  them".
- `docs/README.md` — "**No cross-region results within the host document**".

*The strategy set is described as closed* in two more places, both of which
enumerate exactly `preferred`/`concatenated` and both of which additionally
assert that every other method dispatches `preferred` regardless:

- `docs/language-features.md` — the "When several servers handle one language"
  table and its lead-in ("one of two strategies").
- `docs/README.md` — the `strategy` row of the aggregation table.

And `maxFanOut`'s own description in `docs/README.md` must record that for
`workspace/symbol` it selects names per pair and does **not** cap the query's
total requests or connections.

*And the feature must move* out of `docs/language-features.md`'s "Not currently
provided" list into a section of its own, and into `docs/README.md`'s
bridge-backed request list — carrying the live-only coverage contract and the
fact that coverage can shrink.

One more site is a **related ADR**, and it is wrong rather than merely
incomplete: language-server-bridge-request-strategies states universally that
every other method dispatches `preferred`, and `workspace/symbol` is now a
counterexample. Its per-method table gains no row — that part is only an
omission — but the universal sentence must be corrected.

`docs/README.md`'s cross-layer `layers.aggregation` `strategy` row belongs to
the closed-set list above too: it will schema-accept an inert `union` while
documenting only two values.

### 9. Deferred in this decision

- `workspaceSymbol/resolve` — `resolveProvider` is advertised as `false`. Because
  kakehashi keeps `resolveSupport` absent from the `workspace.symbol` capability
  it declares downstream (point 5), an entry arriving without a range is a
  downstream conformance bug; point 5 drops it.
- `workDoneToken` / `partialResultToken` — neither is forwarded downstream, and
  `workDoneProgress` is left unset on the advertised
  `workspaceSymbolProvider`. The existing client-progress aggregator is keyed by
  region and has no meaning for a request that has no region. Both tokens are
  optional in LSP 3.18.
- **Request coalescing.** A symbol picker typically fires one request per
  keystroke, and this design has no single-flight, debounce, or supersession —
  each keystroke fans out to every live connection. The repo has prior art for
  exactly this failure mode in the semantic-token and diagnostic wire floods.
  Nothing here prevents it; it is left for a follow-up once real traffic exists
  to measure, and is the most likely first regression.
- ~~**The respawn re-open window.**~~ Not deferred — see point 3. A connection
  reaches `Ready` before its virtual documents are replayed, and the barrier is
  awaited rather than skipped.

## Considered Options

**`preferred`, as everywhere else.** Rejected: `preferred` encodes "these are
competing answers to one question", and — because a response cannot be
attributed to a pair — kakehashi has no way to tell competing from
complementary here. Every arbitration it could perform discards symbols on a
basis it cannot compute.

**Allow `preferred` within a single pair, union only across pairs.** Rejected:
it sounds like a smaller change, but "within a pair" does not delimit a coherent
set of results, since a server named by LANG_1's pair can return LANG_2 symbols.
Rejecting it is what collapses the design: once no level arbitrates, the
per-pair walk reduces to a candidate-set walk.

**Attribute each returned symbol to a language by detecting its URI's language,
then apply the owning pair's strategy.** Rejected: it would put language
detection in the fan-in hot path, and it still cannot recover which pair owns a
result — a file may be claimed by several — so it buys a fragile mechanism that
does not answer the question it was built for.

**Reuse `concatenated` instead of adding `union`.** Rejected: `concatenated`
must not deduplicate (see the diagnostics and codeAction defaults), and a symbol
search should. Overloading it would make the existing strategy's contract
conditional on the method.

**Cold-start every configured server so a query covers languages no open file
uses.** Rejected, per point 7. It reads like the thorough option, but it buys no
embedded-block coverage at all, and the real-file coverage it does buy lands on
the `ClientFallback` root alone. It also cannot answer "does this language even
occur in the workspace?" without a workspace file walk the LSP server does not
have.

**Cold-start, and additionally `didOpen` the workspace's files so the spawned
servers have something to index.** Rejected as unbounded: covering embedded code
in unopened files means parsing every candidate host file in the workspace and
opening every extracted region, on a request the user expects to be interactive.
This is the option that shows the cold-start direction has no cheap stopping
point — which is what makes live-only the right cut rather than merely the
cheap one.

**Reuse `transform_location_for_goto` with a synthetic "request virtual URI".**
Rejected: its filter exists to *prevent* cross-region translation, so any
synthetic value either drops everything or defeats the guard.

**Resolve `priorities`/`max_fan_out` from the wildcard
(`languages._._.aggregation["workspace/symbol"]`) entry only.** Rejected: it
gives the feature exactly one knob, but it makes every per-language
`bridge.<key>.aggregation` block a **silent no-op** for this method. Silently
discarding valid configuration is worse than the complexity it avoids.

**Merge every pair's `priorities` into one global ordered allowlist.** Rejected:
a server configured under several pairs would hold several conflicting positions
and several `max_fan_out` caps, with no principled merge. Under `union` the
question dissolves — each pair contributes candidates independently, and the
result set is order-free.

**Honour an explicitly configured `preferred` instead of overriding it.**
Rejected: overriding explicit configuration is a real cost, but silently losing
symbols the user did not know they were excluding is worse, and the override is
announced once at settings-apply time rather than hidden.

**Include `container_name` in the dedup key.** Rejected: LSP documents it as a
UI-only field that cannot be used to re-infer hierarchy, and imposes no format
contract, so two servers describing the same symbol routinely disagree on it.
Including it would defeat dedup on a field carrying no identity.

## Consequences

### Positive

- Symbol search reaches both real project files (via the downstream servers'
  own indexes) and symbols inside embedded code blocks, in one result set.
- The global virtual→host translator is reusable by any future workspace-scoped
  method (call hierarchy, type hierarchy) that faces the same fan-in problem.
- Per-language `aggregation` blocks keep selecting servers for this method — a
  workspace-scoped request does not force users into a separate configuration
  dialect, and no existing block silently becomes a no-op.
- No result is discarded on a basis kakehashi cannot compute. A server
  configured under one language may return another language's symbols, and they
  survive.
- Because nothing arbitrates, there is no per-target response buffer to hold and
  no ordering dependency between targets — the handler is a flat
  fan-out/translate/merge.
- A query never spawns a process, so `Ctrl-T` in a fresh session cannot stampede
  every configured language server.

### Negative

- Results depend on what is open, and coverage can shrink (close, respawn,
  silent connection death). The same query answers differently at different
  points in a session. LSP permits partial `workspace/symbol` results, but a
  user expecting an indexed whole-project search will find this surprising.
- Latency is max-over-live-targets, bounded by the pool's existing 30s request
  timeout and liveness timeout, plus selection's own five-second deadline and
  the reopen barrier's two seconds; forwarded cancellation is best-effort
  because a downstream may ignore it.
- One request per keystroke, un-coalesced (point 9).
- Fan-in can wait on a parse: the distinct host documents a result addresses are
  ensured before translation. Because they are ensured concurrently the pass
  costs about one 200ms wait rather than one per document (point 5).
- An edit landing between that ensure and the freshness check drops the affected
  entries. The window is small but not closed, and the failure is silent.
- A host whose region cache was left empty by a refused populate pass loses its
  embedded symbols **until an unrelated edit forces a reparse** — not just for
  one query (point 5). This is the one accepted failure here that is not
  self-correcting, and it has no signal to the user.
- `max_fan_out` no longer bounds a query's total fan-out — only each pair's
  contribution — so the only real bound on how many connections one query
  touches is how many are live.
- Adding the `Union` variant forces a `Union` arm at 7 exhaustive `match` sites
  in methods that have no use for it.
- A server still `Initializing` when the query arrives is skipped entirely, so
  a search in the seconds after opening a file can miss it (point 3).
- Selection carries its own deadline, because it must take the pool's
  `connections` mutex and other paths hold that across unbounded async work
  (point 6). Expiring it answers from a partial target set.
- `max_fan_out` does not bound this method's total fan-out. It selects names
  per pair, and one name still queries every `Ready`, capable root while a
  connection excluded by one pair re-enters through another. Its documented
  promise to cap concurrent server requests does not hold here, which is why
  the documentation must say so.
- The `kakehashi-virtual-uri-*` filename space is reserved: a real workspace
  file named into it is invisible to symbol search (point 5).
- `strategy` becomes a knob that this one method ignores (with a warning). Users
  who reach for `preferred` to suppress a noisy server must instead leave it out
  of `priorities` — in every pair that names it.
- Dedup is exact-match, so two servers that disagree on a symbol's range both
  survive. The redundant-server case is a config edit, not something the merge
  resolves.
- Whether a downstream includes kakehashi's virtual documents in its own symbol
  index is server-specific — the virtual files do not exist on disk, and servers
  that index only on-disk workspace contents will contribute real-file symbols
  only.
- Shipping this obliges documentation changes, not just additions: both
  user-facing files state the no-cross-block rule as a blanket claim, both
  describe the strategy set as a closed pair, and a related ADR states the
  same closure as a universal rule (point 8).

### Neutral

- `Union` is a named strategy but is meaningful only for this method; elsewhere
  it is normalized to that method's existing default before any handler runs, so
  configuring it there changes nothing. `layers.aggregation["workspace/symbol"]
  .strategy = "union"` is schema-valid and inert for the same reason this method
  never reaches the cross-layer walk.
- Exposing `union` in the serialized config and the generated schema is a
  **one-way door**: it is public API from the first release that ships it, yet
  it offers no choice anywhere — it is mandatory where it applies and inert
  where it does not. Keeping the merge internal to this method would have left
  that door open. It is exposed because a named, inspectable strategy value was
  the maintainer's explicit preference over a hidden merge rule; the cost is
  recorded here so a later reversal is a deliberate deprecation rather than a
  surprise.
- Result ordering is deterministic but not relevance-ranked. LSP delegates
  scoring to the client ("editors will apply their own highlighting and scoring
  on the results"), so a client that re-sorts sees no change and one that does
  not gets a stable order.
- The fan-out and fan-in halves live in different modules
  (`bridge/workspace/symbol.rs` and `lsp_impl/workspace/symbol.rs`) because of
  the `pub(super)` boundaries between them, not because of a design preference.
