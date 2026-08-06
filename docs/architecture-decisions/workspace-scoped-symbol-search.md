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
server reached through one configured pair routinely returns symbols belonging
to another pair's language. Nothing in the response says which configured pair
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

- `BridgeCoordinator::resolve_virtual_uri(uri) -> Option<(host_url, region_id)>`
  is a **global** virtual→host reverse map (it is what `window/showDocument`
  translation already uses), and `resolve_region_offset` rebuilds the
  `RegionOffset` from the live parse for any `(host_url, region_id)` pair.
- `LanguageServerPool::connections()` enumerates the pool's connection map.
- `send_execute_command_on_handle` (`bridge/workspace/execute_command.rs`) is an
  existing precedent for sending a request on a connection **without** a virtual
  document, and it shows exactly what that costs (see point 3).

kakehashi sends downstream no `workspace.symbol` client capability at all
(`bridge/protocol/client_capabilities.rs` sets no `symbol` field on
`WorkspaceClientCapabilities`), so in particular no `resolveSupport`. Per LSP
3.18, a conformant downstream must therefore return a full `Location` rather
than the location-without-range form.

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
   │  { query }    ────▶│ candidate set │─── request ──▶  lua_ls   @ rootA
   │                    │ ∩ live conns  │─── request ──▶  tsgo     @ rootA
   │                    │ dedup by key  │─── request ──▶  tsgo     @ rootB
   │                    │ (fan-out, §3) │
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
   │             │  3. sort   (uri, line, char,   │
   │             │             name, kind)        │
   │             └───────────────┬────────────────┘
   │◀── WorkspaceSymbol[] ───────┘
   │    (always an array, never null)
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
`(uri, start.line, start.character, name, kind)`.

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

Adding the variant is not free: `AggregationStrategy` is matched exhaustively,
with no wildcard arm, at 14 sites across `lsp_impl/bridge_context.rs`,
`text_document/formatting.rs`, `text_document/code_action.rs`, and
`text_document/diagnostic.rs`. Each must gain a `Union` arm, and none of those
methods has a response shape `Union`'s key tuple applies to. Those arms
therefore fall back to each site's existing `Concatenated` behavior: `Union` is
meaningful for `workspace/symbol` and for nothing else. It is a named strategy,
not a generally useful one.

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
about `concatenated`-without-`priorities` for formatting).

Two servers indexing the *identical* root — say two TypeScript servers — are
genuinely competing rather than complementary, and this decision does make their
near-duplicate entries survive when their ranges differ. The lever for that case
is `priorities`, which is an allowlist: name only the server you want. That is a
static, deliberate exclusion the user writes, not an inference kakehashi draws
from an empty response — which is the distinction that makes it acceptable here
and `preferred` not. It does cost the user an edit in **every** pair that names
the unwanted server, because fan-out unions candidates across all pairs.

### 3. Fan-out: live connections first, then the configuration filter

A request has no document, so it cannot pick one `(host_language, bridge_key)`
pair — it walks **all** of them, purely to collect candidates.

The walk mirrors `concatenated_formatting_pairs`, including the part that is
easy to miss: each bridge key must be resolved through
`resolve_with_wildcard(bridge_map, key, merge_bridge_language_configs)` before
`resolve_aggregation(method)`, so that `priorities` / `strategy` /
`max_fan_out` set only on the `bridge._` wildcard still apply to a concrete
pair. Without that step the "no existing configuration becomes a silent no-op"
property this decision claims would not hold. The literal `"_"` key is itself
present in `bridge_map` for essentially every deployment (the shipped defaults
populate `languages._.bridge._`); it is walked like any other key, which is
idempotent under the self-merge and contributes the wildcard's own candidates.

**Order matters, and it is liveness first:**

1. Take the pool's connection map and keep only `Ready` and `Initializing`
   entries. `connections()` returns the raw map, and `ConnectionState` also has
   `Failed`, `Closing`, and `Closed`, which linger until lazily evicted — none
   of them is a valid target.
2. Wait for the surviving `Initializing` entries to reach Ready.
3. **Then** check `has_capability("workspace/symbol")` (see point 4 for why this
   order is forced, and what `has_capability` needs first).
4. **Then** apply the per-pair `priorities` allowlist and `max_fan_out` cap.
5. Dedup the survivors by **connection key** `(server, root)`.

Applying the cap before the liveness filter would let dead or absent servers
consume cap slots and silently exclude a live server ranked below the cutoff —
which would contradict this decision's own "coverage is what is live" contract.

`priorities` is an allowlist: listed servers are candidates, `"*"` stands for
the rest, and an explicit `[]` remains the per-method kill switch. Its **order**
still decides which N survive a `max_fan_out` cap (`truncate_entries` keeps the
highest-priority N in walk order); only the *arbitration* meaning of order is
dropped. A `_self` pair contributes candidates only when
`bridge._self.enabled = true` for that language — `is_host_bridging_enabled` is
a **direct** lookup with no wildcard fallback, unlike the aggregation fields
beside it, so a `bridge._self.aggregation` block without `enabled` contributes
nothing at all.

Dedup is by connection key, not server name: the response depends on the
connection's own indexed workspace, so a connection named by several pairs is
asked **once**, while the same server name under two roots is genuinely two
connections and two requests.

```
  settings.languages × bridge_map.keys()      ← candidate walk only,
        │  (each key via resolve_with_wildcard) no arbitration
        ▼
  ┌──────────────────────┐   ┌──────────────────────┐
  │ (LANG_1, _self)      │   │ (LANG_2, _self)      │  ... every pair
  │ priorities ["B"]     │   │ priorities ["C"]     │
  └──────────┬───────────┘   └──────────┬───────────┘
             └────────────┬─────────────┘
                          ▼
        ┌──────────────────────────────────────┐
        │ 1. keep Ready + Initializing only    │
        │    (NOT Failed/Closing/Closed)       │
        │ 2. wait for Ready                    │
        │ 3. THEN has_capability               │
        │ 4. THEN allowlist + max_fan_out      │
        │    NEVER spawns a connection         │
        └──────────────────┬───────────────────┘
                           ▼
        ┌──────────────────────────────────────┐
        │ dedup by CONNECTION KEY (server,root)│
        │   (B, rootA)  ← named by LANG_1      │
        │   (B, rootB)  ← same server, another │
        │                 root: 2 connections  │
        │   (C, rootA)  ← named by LANG_2      │
        └──────────────────┬───────────────────┘
                           ▼
              one request per connection (§4),
              then per-entry fan-in (§5),
              then UNION → dedup → sort (§1)
```

### 4. The send lives in the bridge layer; the translation does not

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
- **To be called after the Ready wait.** It falls back to
  `server_capabilities()`, which is `None` until `set_server_capabilities` runs
  during the handshake — so prefiltering an `Initializing` connection drops
  exactly the connections point 3 decided to keep.

Cancellation needs nothing new: `register_upstream_request` already holds many
`(server, root)` keys per upstream id, `forward_cancel_by_upstream_id_if_current`
already iterates all of them, and `UpstreamRegistrySweepGuard` unregisters the
whole entry. Multi-target cancellation falls out of using the pattern.

Fan-in cannot live in the bridge layer, because `resolve_region_offset` is
`pub(super)` to `lsp_impl`. So unlike every `transform_*_response_to_host`
function — which is pure because the caller already resolved *the* offset — this
translator resolves a different offset per entry and must sit where the
`DocumentStore` / `LanguageCoordinator` / `BridgeCoordinator` handles are, as
`ShowDocumentTranslator` does. The bridge layer returns each target's raw
result; `lsp_impl` classifies and translates. The classification is testable as
a pure function only if the resolver is injected.

### 5. Fan-in: a global virtual→host translator

Every entry is classified independently. Because each one resolves **its own**
`(host_url, region_id, offset)`, this path may cross blocks where the goto path
may not: the goto filter exists because only one region's offset is in hand.

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
     resolve_virtual_uri(uri)
               │
               ├── None ─────────▶ DROP  (no host mapping for this URI)
               │
               ▼ Some((host_url, region_id))
     resolve_region_offset(host_url, region_id)
               │
               ├── None ─────────▶ DROP  (region invalidated by edits, or
               │                          host document closed)
               ▼ Some(offset)
     TRANSLATE
       uri   := host_url
       range := translate_virtual_range_to_host(range, offset)
```

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

**Parse freshness is load-bearing here.** `resolve_region_offset` reads the live
parse, and `didChange` clears the tree and reparses off-ingress, so during the
reparse window it returns `None` for every region of the edited document — which
this classifier would silently turn into "no embedded symbols". The
whole-document handlers avoid this by calling `ensure_document_parsed` first;
this method has no target document to name. It therefore calls
`ensure_document_parsed` for **every open host document** before consuming
responses. That set is bounded by what the client has open (virtual documents
exist only for open hosts), and the pre-warm is issued concurrently with the
fan-out requests, so it costs latency only when a parse is genuinely in flight.

The response to the client is always an **array**, never `null`, so "no server
was running" and "everything was dropped" are not distinguished — the spec
assigns no distinct meaning to `null` here, and an array keeps the empty case
uniform.

Entries are emitted as `WorkspaceSymbol[]` — `WorkspaceSymbolResponse::Nested`
in `ls-types`, whose variant names are a misnomer: **both** variants are flat
arrays, and the choice is the element type, not hierarchy (there is no nested
form for this method; `containerName` is spec-documented as unusable for
re-inferring one). `SymbolInformation[]` is the deprecated alternative and is
not emitted. No client capability governs the choice, so none is consulted.

### 6. Latency is bounded by the pool's timeouts, not by cancellation

Every target is awaited, so latency is max-over-targets. The bounds are:

- `wait_for_response` wraps each request in a hardcoded **30-second** timeout and
  removes the router entry when it fires.
- The reader's **liveness timeout** can independently fail a connection that has
  gone silent, transitioning it to `Failed`.
- `$/cancelRequest` is forwarded to every in-flight target, but LSP explicitly
  permits a downstream to ignore `$/` notifications, so it is best-effort and
  cannot be the guarantee.

No *additional* per-request deadline is introduced. Because no target is ever
cold-started (point 7), the practical worst case is bounded by servers that are
already running and already answering other requests.

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
one region's offset is known. Workspace symbol search resolves each result's
region independently, so the offset is always the right one for the entry being
translated.

Two user-facing docs state the no-cross-block rule as a blanket claim and must
be amended, not merely appended to — the wrong part is the framing sentence in
each: `docs/language-features.md` ("Bridged features are also limited to
embedded code blocks in one respect: navigation and edits do not cross between
blocks", and separately "features that need to see across blocks do not work
between them") and `docs/README.md` ("**No cross-region results within the host
document**"). Both files' itemized bodies are already correctly scoped to the
goto/references/rename transforms and stay true. The
language-server-bridge-request-strategies per-method table gains no row for this
method and is left incomplete rather than wrong.

### 9. Deferred in this decision

- `workspaceSymbol/resolve` — `resolveProvider` is advertised as `false`. Because
  kakehashi declares no `workspace.symbol` capability downstream, an entry
  arriving without a range is a downstream conformance bug; §5 drops it.
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
- **The respawn re-open window.** A connection reaches `Ready` *before* its
  virtual documents are replayed; the barrier that signals replay completion
  (`wait_for_pending_reopen`) is today awaited only by `workspace/executeCommand`.
  A query landing in that window under-reports a respawned server's
  embedded-block symbols, indistinguishably from "no matches". Awaiting the
  barrier per target would add unbounded latency to every query, so this is
  accepted rather than fixed.

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
- Latency is max-over-live-targets, bounded only by the pool's existing 30s
  request timeout and liveness timeout; forwarded cancellation is best-effort
  because a downstream may ignore it.
- One request per keystroke, un-coalesced (point 9).
- Adding the `Union` variant forces a `Union` arm at 14 exhaustive match sites
  in methods that have no use for it.
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

### Neutral

- `Union` is a named strategy but is meaningful only for this method; elsewhere
  it behaves as `Concatenated`.
- Result ordering is deterministic but not relevance-ranked. LSP delegates
  scoring to the client ("editors will apply their own highlighting and scoring
  on the results"), so a client that re-sorts sees no change and one that does
  not gets a stable order.
- The fan-out and fan-in halves live in different modules
  (`bridge/workspace/symbol.rs` and `lsp_impl/workspace/symbol.rs`) because of
  the `pub(super)` boundaries between them, not because of a design preference.
