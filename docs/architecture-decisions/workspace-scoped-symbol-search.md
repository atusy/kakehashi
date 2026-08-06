# Workspace Scoped Symbol Search

**Related Decisions**: [cross-layer-aggregation](cross-layer-aggregation.md),
[language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md),
[language-server-bridge-request-strategies](language-server-bridge-request-strategies.md),
[aggregation-priorities-wildcard](aggregation-priorities-wildcard.md),
[host-document-bridge](host-document-bridge.md)

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
- **Layer arbitration** is `walk_layers`, first-non-empty-wins (`preferred`).

None of the three survives the loss of the document. There is no injection
language to select servers with, no single virtual URI to translate against, and
`preferred` would let whichever layer answers first hide the other layer's
symbols entirely — which for a *search* is a silent loss of results, not a
tie-break.

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
   walk_layers → first non-empty layer wins (preferred)


WORKSPACE-SCOPED  (workspace/symbol)

  params.query                        ← no textDocument at all
         │
         ✗ no host document   ✗ no region   ✗ no offset   ✗ no virtual URI
         │
         ├─▶ FAN-OUT cannot be selected by injection language
         ├─▶ FAN-IN has no "request virtual URI" to filter against
         └─▶ preferred would let one layer hide the other layer's symbols
```

Two facts in the existing code make the feature tractable anyway:

- `BridgeCoordinator::resolve_virtual_uri(uri) -> Option<(host_url, region_id)>`
  is a **global** virtual→host reverse map (it is what `window/showDocument`
  translation already uses), and `resolve_region_offset` rebuilds the
  `RegionOffset` from the live parse for any `(host_url, region_id)` pair.
- `LanguageServerPool::connections()` enumerates every live connection, and
  `ConnectionHandle` exposes `has_capability` / `send_request` /
  `wait_for_response`, so a request can be sent to a connection without going
  through `get_or_create_connection`.

kakehashi does **not** declare `workspace.symbol.resolveSupport` in the
capabilities it sends downstream (`bridge/protocol/client_capabilities.rs`), so a
conformant downstream must return `WorkspaceSymbol`s carrying a full `Location`
rather than the 3.17 location-less form.

## Decision

Implement `workspace/symbol` as a **workspace-scoped** bridged request with its
own aggregation strategy, its own fan-out enumeration, and a global fan-in
translator. Defer `workspaceSymbol/resolve`.

```
 client                    kakehashi                    downstream servers
   │
   │ workspace/symbol   ┌───────────────┐
   │  { query }    ────▶│ candidate set │─── request ──▶  lua_ls   @ rootA
   │                    │ dedup by      │─── request ──▶  tsgo     @ rootA
   │                    │ connection    │─── request ──▶  tsgo     @ rootB
   │                    │ (fan-out, §3) │─── request ──▶  pyright  @ rootA
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
   │             │  2. dedup  (name, kind, uri,   │
   │             │             range, container)  │
   │             │  3. sort   (uri, line, char,   │
   │             │             name, kind)        │
   │             └───────────────┬────────────────┘
   │◀── Flat | Nested ───────────┘
   │    (per client capability)
   │
   │ $/cancelRequest ─▶ forwarded to every in-flight target. This is the
   │                    ONLY bound on latency — no deadline is imposed.
   │                    Practical because no target is ever cold-started (§4).
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
`(name, kind, uri, range, container_name)`, then **sort deterministically** by
`(uri, start.line, start.character, name, kind)`.

It is a distinct value rather than a reuse of `Concatenated` because
`Concatenated` deliberately preserves duplicates and source ordering (diagnostics
and code actions from different servers are complementary and each must survive).
A symbol search is set-valued: the same real file indexed by two servers, or by
one server under two workspace roots, must appear once. Sorting is part of the
strategy, not a caller detail — `JoinSet` completion order is nondeterministic
and an unstable result order is a self-inflicted test flake.

`Union` is the **only** strategy for `workspace/symbol`, at both the bridge level
and the layer level. It is the default, and an explicitly configured `preferred`
or `concatenated` is overridden back to it with a warning (point 2).

### 2. `preferred` is wrong for this method at every level

The repo's own criterion for choosing a strategy is stated at
`default_aggregation_strategy_for_method`: code actions concatenate because,
"unlike formatting (**competing** whole-document edits), code actions from
different servers are **complementary**."

Two servers' `workspace/symbol` responses are complementary in exactly that
sense — they index different parts of the workspace. `preferred` would discard
one of them.

The reason it cannot be rescued is that **a `workspace/symbol` response is not
attributable to a language**. A server answers "here are the workspace's symbols
matching `query`"; a server configured only under LANG_1 may perfectly well
return LANG_2 symbols, because what it indexes is its workspace root, not a
language. kakehashi therefore cannot decompose one response into the share that
"belongs to" LANG_1 and the share that "belongs to" LANG_2. Concretely, given

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
`union`, with one warning at settings-apply time. This follows the existing
misconfiguration path (`misconfigured_settings_warnings`, which already warns
about `concatenated`-without-`priorities` for formatting) rather than silently
discarding symbols the user never intended to lose.

### 3. Fan-out: a candidate-set walk, deduplicated by connection

Because every level unions, there is no per-target arbitration left to do, and
the configuration walk collapses to a **candidate-set** walk. A request has no
document, so it cannot pick one `(host_language, bridge_key)` pair — it walks
**all** of them (`settings.languages` × `bridge_map.keys()`, exactly as
`concatenated_formatting_pairs` already does) purely to collect candidates.

`priorities` and `max_fan_out` still filter, per pair:

- `priorities` is an **allowlist**: listed servers are candidates, `"*"` stands
  for the rest, and an explicit `[]` remains the per-method kill switch.
- Its **order** still matters when `max_fan_out` caps the candidates — the cap
  keeps the highest-priority N. Order is what nothing else can express; it is
  only the *arbitration* meaning of order that this method drops.
- A `_self` pair contributes candidates only when
  `bridge._self.enabled = true` for that language. `is_host_bridging_enabled` is
  a **direct** lookup with no wildcard fallback, so a `bridge._self.aggregation`
  block without `enabled` contributes nothing at all.

The candidate list is then **intersected with the live connections** (point 4):
a query never spawns a server. What survives is deduplicated by **connection
key** `(server, root)`, not by server name — the response is a function of
`query` alone, so a connection named by several pairs is asked **once**. If two
pairs resolve the same server name to configs producing different connection
keys, those are genuinely two connections and two requests.

```
  settings.languages × bridge_map.keys()      ← candidate walk only,
        │                                       no arbitration
        ▼
  ┌──────────────────────┐   ┌──────────────────────┐
  │ (LANG_1, _self)      │   │ (LANG_2, _self)      │  ... every pair
  │ priorities ["B"]     │   │ priorities ["C"]     │
  └──────────┬───────────┘   └──────────┬───────────┘
             │                          │  apply allowlist + max_fan_out
             └────────────┬─────────────┘
                          ▼
        ┌──────────────────────────────────────┐
        │ ∩ live connections — NEVER spawns    │
        │   pool.connections(), Ready or       │
        │   Initializing, has_capability       │
        └──────────────────┬───────────────────┘
                           ▼
        ┌──────────────────────────────────────┐
        │ dedup by CONNECTION KEY (server,root)│
        │   (B, rootA)  ← named by LANG_1      │
        │   (B, rootB)  ← same server, another │
        │                 root: 2 connections  │
        │   (C, rootA)  ← named by LANG_2      │
        │   B@rootA is asked ONCE even if      │
        │   several pairs name it              │
        └──────────────────┬───────────────────┘
                           ▼
              one request per connection, sent on the
              handle — no re-acquire, no spawn
                           │
                           ▼
              per-entry fan-in (§5), then
              UNION → dedup → sort  (§1)
```

### 4. Coverage is what is live, and a query never cold-starts a server

A candidate with no live connection is **skipped**, not spawned. Coverage is
therefore "the servers that are running because of what the client has opened",
and it grows when the client opens more files — opening a host document spawns
its servers and opens its virtual documents, so the next query sees more.

This is not merely a cost trade. Cold-starting cannot deliver the coverage it
appears to promise:

- **Embedded-block symbols cannot be reached by spawning at all.** A virtual
  document does not exist on disk, so a downstream learns of it only through
  `didOpen`, and kakehashi opens virtual documents only for host documents the
  *client* has opened. Reaching embedded code in unopened files would mean
  parsing every candidate host file in the workspace and opening every region —
  precisely the unbounded work this method must not do. Spawning a server buys
  exactly zero embedded-block coverage, which is kakehashi's own contribution.
- **Real-file symbols would be reached, but at one root only.** A server spawned
  without a document hint resolves to the `ClientFallback` key
  (`resolve_acquire`: "no document hint … stays on the client-root fallback"),
  so a multi-root workspace gets the client root and none of the marker-derived
  per-root connections. The coverage gained is partial and hard to explain.
- **Scoping by "does this language occur in the workspace?" would need a
  mechanism that does not exist.** The LSP server is document-driven and holds
  no workspace index — nothing walks the filesystem on the server path (the
  `ignore` crate is used only by the `src/cli/files.rs` subcommand). Under the
  live-only rule that question answers itself for free: a language with no open
  file has no live server and is not queried.

Connections that are live but still `Initializing` are **included**, waiting for
Ready with the existing acquisition timeout. They were already being paid for,
and excluding them would make a query issued just after opening a file depend on
timing — a nondeterminism this codebase does not need more of.

Two ordering constraints follow from that choice, and getting either wrong
silently returns no results rather than failing:

- **Capability prefiltering must happen *after* the Ready wait, not before it.**
  `has_capability` falls back to `server_capabilities()`, which is `None` until
  `set_server_capabilities` runs during the handshake. Prefiltering an
  `Initializing` connection therefore drops exactly the connections this section
  just decided to keep.
- **`has_capability` needs a `workspace/symbol` arm.** Its `match` ends in
  `_ => false`, so an unlisted method reports *every* server as incapable. The
  arm reads `workspace_symbol_provider: Option<OneOf<bool,
  WorkspaceSymbolOptions>>` in the same shape as the existing
  `textDocument/definition` arm.

### 5. Fan-in: a global virtual→host translator over four result classes

A pure function over the downstream's `serde_json::Value` response classifies
every returned entry by its URI:

```
  one entry of the downstream's result array
             │
             ▼
     ┌───────────────────┐  no
     │ is_virtual_uri ?  ├─────▶ REAL FILE  ─▶ pass through untouched
     └─────────┬─────────┘                     (external definition, a
               │ yes                            file the server indexed)
               ▼
     ┌───────────────────┐  yes
     │ is_scratch_uri ?  ├─────▶ DROP  (a non-region virtual document;
     └─────────┬─────────┘              names no place in any host file)
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
               │                          host document closed — a virtual
               │                          URI reaching the editor names a
               ▼ Some(offset)              file that is not on disk)
     TRANSLATE
       uri   := host_url
       range := translate_virtual_range_to_host(range, offset)
```

Every entry resolves **its own** `(host_url, region_id, offset)`. That is the
whole reason this path may cross blocks where the goto path may not: the goto
filter exists because only one region's offset is in hand, and here each entry
brings its own.

Dropping — rather than passing through — an unresolvable virtual URI is
deliberate: a virtual URI that escapes to the editor names a file that does not
exist on disk, so the symbol is unopenable. This mirrors `window/showDocument`
translation, which drops the selection it cannot translate.

### 6. This is the first deliberate cross-block feature

Every other bridged navigation path filters out results addressed to a region
other than the request's own, because a cross-region offset is unsafe when only
one region's offset is known. Workspace symbol search resolves **each** result's
region independently, so the offset is always the right one for the entry being
translated. `docs/language-features.md`'s blanket "navigation and edits do not
cross between blocks" claim is amended accordingly.

### 7. Deferred in this decision

- `workspaceSymbol/resolve` — `resolveProvider` is advertised as `false`. Because
  kakehashi does not declare `resolveSupport` downstream, entries arriving
  without a resolvable `Location` are a downstream conformance bug; they are
  dropped rather than surfaced unresolvable.
- `workDoneToken` / `partialResultToken` — neither is forwarded downstream. The
  existing client-progress aggregator (`mint_region_progress_source`) is keyed by
  region and has no meaning for a request that has no region. Both tokens are
  optional in LSP 3.18.

## Considered Options

**`preferred`, as everywhere else.** Rejected at every level, per point 2:
`preferred` encodes "these are competing answers to one question", and two
servers' workspace symbol sets are complementary, not competing.

**Keep `preferred` across groups but allow it *within* a group.** Rejected —
this was an intermediate position, and it does not survive the attribution
argument. A server named by LANG_1's group can return LANG_2 symbols, so
"within a group" does not delimit a coherent set of results to arbitrate over.
Rejecting it is what collapses the group model: once no level arbitrates, the
per-pair walk reduces to a candidate-set walk (point 3).

**Attribute each returned symbol to a language by detecting its URI's language,
then apply the owning language's strategy.** Rejected: it would put language
detection in the fan-in hot path, and it still cannot recover which *pair* owns
a result — a file may be claimed by several — so it buys a fragile mechanism
that does not answer the question it was built for.

**Reuse `concatenated` instead of adding `union`.** Rejected: `concatenated`
must not deduplicate (see the diagnostics and codeAction defaults), and a symbol
search must. Overloading it would make the existing strategy's contract
conditional on the method.

**Cold-start every configured server so a query covers languages no open file
uses.** Rejected, per point 4. It reads like the thorough option, but it buys no
embedded-block coverage at all (those need `didOpen` of virtual documents that
only an open host document produces), and the real-file coverage it does buy
lands on the `ClientFallback` root alone. It also cannot answer "does this
language even occur in the workspace?" without a workspace file walk the LSP
server does not have.

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
`bridge.<key>.aggregation` block a **silent no-op** for this method. A user who
writes `[languages.LANG_1.bridge._self.aggregation] workspace/symbol = {...}`
would see it ignored with no diagnostic. Silently discarding valid configuration
is worse than the complexity it avoids.

**Merge every pair's `priorities` into one global ordered allowlist.** Rejected:
a server configured under several pairs would hold several conflicting positions
and several `max_fan_out` caps, with no principled merge. Under `union` the
question dissolves — each pair contributes candidates independently, and the
result set is order-free — so nothing has to be reconciled.

**Walk every pair for candidates only, and union the results** (chosen). The
per-pair walk is not new: `concatenated_formatting_pairs` already enumerates
`settings.languages` × `bridge_map.keys()` and calls `resolve_aggregation(method)`
on each. Per-pair `priorities` and `max_fan_out` keep working, so no existing
configuration becomes a silent no-op, and no result is discarded on a basis
kakehashi cannot compute.

**Honour an explicitly configured `preferred` instead of overriding it.**
Rejected by the maintainer: overriding explicit configuration is a real cost,
but silently losing symbols the user did not know they were excluding is worse,
and the override is announced once at settings-apply time rather than hidden.

## Consequences

### Positive

- Symbol search reaches both real project files (via the downstream servers'
  own indexes) and symbols inside embedded code blocks, in one result set.
- The global virtual→host translator is reusable by any future workspace-scoped
  method (call hierarchy, type hierarchy) that faces the same fan-in problem.
- `union` is available as an explicit, user-selectable strategy for other
  methods.
- Per-language `aggregation` blocks keep selecting servers for this method — a
  workspace-scoped request does not force users into a separate configuration
  dialect, and no existing block silently becomes a no-op.
- No result is ever discarded on a basis kakehashi cannot compute. A server
  configured under one language may return another language's symbols, and they
  survive.
- Because nothing arbitrates, there is no per-target response buffer to hold
  and no ordering dependency between targets — the handler is a flat
  fan-out/translate/merge, which is why this is a smaller change than an
  arbitrating design.
- A query never spawns a process. Latency is bounded by the servers already
  running, so declining to impose a deadline stays practical, and `Ctrl-T` in a
  fresh session cannot stampede every configured language server.
- Coverage is explainable in one sentence — "the servers running because of what
  you have opened" — and it grows monotonically as the client opens files, with
  no separate indexing mode to understand or invalidate.

### Negative

- Latency is max-over-live-servers, not first-win: the response waits for every
  target. No deadline is imposed in this version; the client's `$/cancelRequest`
  is the only bound.
- Results depend on what is open. A language whose files the user has not opened
  contributes nothing, and the same query answers differently later in a
  session. LSP permits partial `workspace/symbol` results, but a user expecting
  an indexed whole-project search will find this surprising.
- `strategy` becomes a knob that this one method ignores (with a warning). Users
  who reach for `preferred` to suppress a noisy server must instead leave it out
  of `priorities`, which is the allowlist that already expresses exactly that.
- Whether a downstream includes kakehashi's virtual documents in its own symbol
  index is server-specific — the virtual files do not exist on disk, and servers
  that index only on-disk workspace contents will contribute real-file symbols
  only.

### Neutral

- The `union` strategy is selectable for other methods, but no other method
  defaults to it.
- Result ordering is defined by the strategy, so clients that re-sort see no
  change and clients that do not get a stable order.
