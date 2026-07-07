# Cross-Layer Semantic Token Merge

**Related Decisions**:
- [cross-layer-aggregation](cross-layer-aggregation.md) — The two-stage layer model and `layers.aggregation.<method>.priorities` ordering this decision reuses; its "Out of Scope → Semantic tokens" bullet deferred exactly the design recorded here
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Strategy 1 (parallel fetch with progressive refinement) is the temporal half of this merge
- [semantic-token-overlap-resolution](semantic-token-overlap-resolution.md) — The native-layer sweep line this decision lifts to an inter-layer dimension; its Phase 4 (`overlappingTokenSupport`) bounds what single-winner-per-region gives up
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document coordinates that virt tokens are remapped from
- [host-document-bridge](host-document-bridge.md) — The host layer (`bridge._self`) whose tokens enter on real coordinates
- [language-server-bridge](language-server-bridge.md) — Spawn-on-injection-detection timing (post-`initialize`) behind the "legends unknown at `initialize`" constraint

## Implementation Status

**Aspirational — deferred with a specified design and trigger.** Semantic
tokens are **native-only today**: `textDocument/semanticTokens/{full,range}` is
not bridged (language-server-bridge-request-strategies marks the semanticTokens row ❌ Not
implemented), and
cross-layer-aggregation deliberately scoped semantic tokens out of its
`preferred`/`concatenated` mechanism. This decision does **not** schedule
implementation. It records *how* the merge will work when token bridging is
built, so that the scoping-out has a concrete successor instead of an open
question, and so reviewers of any future token-bridge PR have a fixed target.

**Trigger to implement**: a bridged language server demonstrably produces
semantic tokens richer than kakehashi's tree-sitter output for a real
host/injection pair (the motivating case is rust-analyzer on a Rust host
document), *and* token bridging (Strategy 1's parallel fetch + refresh
plumbing) lands. Until both hold, native-only remains correct and this file is
design-ahead-of-code.

## Context

A single `textDocument/semanticTokens/full` request can, once token bridging
exists, be answered by the same three result layers cross-layer-aggregation
defined for position and document-wide methods:

1. **native** — kakehashi's tree-sitter tokens (host document + injections),
   already resolved into a non-overlapping set by
   semantic-token-overlap-resolution's two-stage `finalize_tokens`.
2. **host** — `bridge._self` tokens from a host-language server (e.g.
   rust-analyzer on a `.rs` document, marksman on Markdown), on the real URI.
3. **virt** — `bridge.<inj>` tokens from an injection-language server (e.g.
   pyright on a Python fenced block), on a synthesized virtual URI.

cross-layer-aggregation **excluded** semantic tokens from its generic
mechanism, with the reason: the progressive-refinement strategy is a *temporal*
merge (native immediately, bridged tokens replacing them later), not an
ordering, and — in wording this decision now replaces with a direct
reference — "a future `merged`-style strategy may bring them in." This is
that decision.

### Why `preferred`/`concatenated` cannot express this

Every other bridged method returns a **set of independent items** — completion
entries, diagnostics, locations. `concatenated` appends them; `preferred`
returns the first non-empty layer. Both are list operations.

Semantic tokens are not a list. They are a **spatial tiling of the document**:
the LSP wire format is delta-encoded and (absent `overlappingTokenSupport`)
must be position-sorted and non-overlapping. Three layers each emit a partial
tiling of the *same* coordinate space, so they compete for the same characters.

- `concatenated` (append) produces overlapping, out-of-order tokens — an
  invalid delta stream, not a merge.
- `preferred` (first non-empty layer wins wholesale) throws away the native
  tiling everywhere the chosen layer is silent (host servers do not tokenize
  injection blocks; virt servers cover only their own block), leaving gaps.

The correct operation is **per-region winner selection across layers** — the
sweep line semantic-token-overlap-resolution already runs *within* the native
layer (host vs. injection by `depth`), lifted one dimension to rank *layers*.
That is a distinct third operation, which this decision names `merged`.

## Decision Drivers

- **Preserve instant highlighting.** Native tree-sitter tokens are local and
  fast; bridged tokens arrive 10s–1000s of ms later. The user must see
  highlighting immediately, so the merge is inherently *temporal*, not a
  request that blocks on all layers (language-server-bridge-request-strategies Strategy 1).
- **One advertised legend, fixed at `initialize`.** LSP binds the
  `SemanticTokensLegend` when the capability is registered — for kakehashi's
  static registration, at `initialize`. (Re-registering with a new legend via
  `client/registerCapability` exists in the protocol, but only when the client
  opts into `dynamicRegistration` — optional support the merge cannot stand
  on; see Alternative D.) Bridged servers are spawned
  **only after** kakehashi has already initialized — on injection detection
  during parsing of an already-open document (language-server-bridge;
  ls-bridge-async-connection handles the async I/O) — so their legends are
  unknown when kakehashi must declare its own. The merge cannot widen the
  legend per server at runtime; the only extension point is static — entries
  the user declares in config, known before `initialize` (§3).
- **Flat non-overlapping output by default.** Unless the client sets
  `overlappingTokenSupport`, exactly one token may cover each character — so a
  per-region *single winner*, not a stack.
- **Reuse, don't fork, the machinery.** Layer ranking
  (`layers.aggregation.<m>.priorities`), the sweep line, and virt→host
  coordinate remapping all exist. The new surface should be a thin top layer
  over them, not a parallel pipeline.
- **Keep the generic aggregation namespace clean.** `merged` is meaningful for
  exactly two methods and needs temporal + legend machinery the other methods
  never touch. It must not become a `preferred`/`concatenated`-style value
  users can write on arbitrary methods or at stage 1.

## Decision Outcome

**Chosen approach**: semantic-tokens methods compose layers with a dedicated
`merged` operation — a *two-level* token resolution followed by *temporal*
refinement — driven by the existing `layers.aggregation.<m>.priorities`
ordering. `merged` is **not** a user-writable `AggregationStrategy` value; it is
the internal, only composition for semantic-tokens methods.

### 1. Two-level resolution (spatial)

```text
native finalize          host server          virt server(s)
(existing two-stage   ┐  (one non-overlapping  one set per injection,
 sweep → one          │   set, real coords)     virtual coords
 non-overlapping set) │            │                  │
        │             │            │           remap virt→host coords
        │             │            │           + collapse the per-injection
        │             │            │             streams deepest-first into
        │             │            │             ONE non-overlapping virt set
        └─────────────┴────────────┴──────────────────┘
                           │
                           ▼
            Inter-layer sweep line (NEW, thin)
            primary key  = layer rank from priorities
            tie-break    = (each layer collapsed to one
                            non-overlapping stream in level 1)
                           │
                           ▼
            One non-overlapping set, host coords → delta encode
```

- **Level 1 (intra-layer) collapses each layer to exactly one non-overlapping
  stream.** Native is the existing `finalize_tokens` output. Host is
  `bridge._self`'s contribution: a single server's response is non-overlapping
  by protocol (kakehashi does not advertise `overlappingTokenSupport` to
  downstream servers); when a bridge target dispatches to **multiple
  servers**, their per-server streams are collapsed by the same breakpoint
  sweep with the server's rank in the stage-1 `bridge.<key>.aggregation`
  `priorities` ordering as the key — the first-listed server that tokenized a
  region wins it. A `"*"` entry ranks every unlisted server at its own
  position as one group (aggregation-priorities-wildcard's first-win rest
  group); the sweep needs a total order, so ties inside the group break by
  **server name**, matching the by-name sort the bridge coordinator already
  imposes on candidate lists for determinism
  (`src/lsp/bridge/coordinator.rs`) — not
  registration order, which is a `HashMap` iteration artifact and unstable.
  Users wanting a specific intra-group ranking list those servers explicitly
  before `"*"`. For semantic
  tokens this **supersedes** language-server-bridge-request-strategies' provisional "Later server
  wins for overlapping ranges" rule, which predates this decision. The same
  per-target collapse applies within each injection's
  `bridge.<inj>` before the cross-injection nesting collapse below, so every
  downstream stream enters that step already single. **Virt is not a single
  response** — nested injections (markdown → python → sql) yield one stream
  *per injection*, and a deeper injection's host range sits inside its
  parent's, so the virt streams overlap each other. Level 1 therefore collapses
  the virt streams into one non-overlapping set **deepest-first** (the sql
  fragment wins its sub-range, python fills the rest), reusing the native
  "deeper wins" depth convention (cross-layer-aggregation "nested injections
  stay implicit"). Only after this collapse is virt a single stream, so level 2
  never sees an intra-virt tie. **Normalization before level 2:** all bridged
  tokens are legend-translated (§3), coordinate-remapped (host: identity; virt:
  virtual→host), and **split into the sweep's single-line representation** —
  the same normalization `split_multiline_tokens` performs for native multiline
  tokens (semantic-token-overlap-resolution "Multiline Token Handling"). Empty
  fragments produced by clipping/splitting are dropped before the sweep, as
  native finalize already retains only `length > 0`. The sweep operates per
  line, so every layer must reach it as single-line fragments.
- **Level 2 (inter-layer) is the new, thin sweep.** It runs the same
  breakpoint sweep as semantic-token-overlap-resolution, but the **primary
  priority key is the layer's rank in `priorities`** (most significant), so in
  any interval the highest-ranked layer that produced a token wins outright.
  Because level 1 collapsed each of the three layers to one non-overlapping
  stream, there is no intra-layer tie to break at this level.
- **Coordinate space.** virt tokens are remapped virtual→host before the sweep
  (the translation virt already performs for `definition`); host and native are
  already on real coordinates. The sweep operates entirely in host coordinates.
  Remap invariants: a remapped virt token is **clipped to its originating
  injection's host range** (so a token never leaks past the block it came
  from), and any token that cannot be expressed as a **single contiguous host
  range** — e.g. one straddling a non-contiguous injection's excluded gap — is
  split at the gap or dropped before the sweep. This mirrors how
  `compute_included_ranges` already treats injection gaps and keeps the sweep's
  inputs well-formed.

The default order `priorities = ["virt", "host", "native"]` makes this degrade
to the existing "deeper wins" convention: a bridged injection token beats the
native tree-sitter token in the same block; native fills the regions it
tokenizes wherever higher-priority layers are silent. (Semantic tokens are
sparse, so the merged set covers only characters *some* layer tokenized —
"gap-free" means the merge adds no *new* gaps via wholesale layer selection,
not that every character is tokenized.) Dropping a layer from `priorities`
suppresses it for the method (allowlist semantics, identical to
cross-layer-aggregation) — e.g.
`["host", "native"]` ignores injection servers; `["native"]` is exactly
today's behavior.

### 2. Temporal refinement (progressive)

The merge is not one-shot. It adopts language-server-bridge-request-strategies Strategy 1's
parallel-fetch-with-progressive-refinement shape, but **overrides** one choice
Strategy 1 makes: where Strategy 1 uses whichever response wins the race
(bridged directly if it arrives first), this merge is **deliberately
native-first** — the first response
never waits on a race outcome. Native tokens are local and effectively always
ready first; tying the instant-paint guarantee to a race would make the
zero-latency experience nondeterministic for no benefit. (Native-first
presumes `native` participates, the default; the qualification below covers a
config that suppresses it.)

1. The first response to a `semanticTokens/full` request returns the
   **synchronously-available set immediately** — when `native` is in
   `priorities` (the default): the native set, merged with any bridged sets
   already on hand (current, or stale-shifted per the version contract below);
   bridged layers still in flight are not awaited even if one could return
   first. If `priorities` **omits** `native`
   (e.g. `["host"]`), there is no instant local layer: the first response is
   whatever bridged sets are already on hand (current, or stale-shifted per
   the version contract below), or an empty result if none are — and the
   bridged layers still refine via step 2. Omitting
   native therefore trades instant highlighting for bridged-only output; the
   default keeps the instant-paint guarantee. Config validation **warns** when
   `priorities` omits `native`, so the trade is explicit, not accidental.
2. When a bridged layer responds, kakehashi recomputes the level-2 sweep and
   emits `workspace/semanticTokens/refresh`; the client re-requests and
   receives the merged set (as a `full/delta` where the client supports it).
   A refresh fires only when the newly accepted contribution **changes the
   merged result** for the current snapshot; a response identical to the
   layer's already-accepted contribution is absorbed silently. Symmetrically,
   a request does **not** start a new fetch for a layer whose accepted
   contribution was **computed by its server against the current snapshot** —
   source freshness, a deliberately stricter test than the display currency a
   stale-shifted set has. A shifted set is current enough to *serve* (§2's
   version contract) but never suppresses the re-fetch that repairs its
   dropped and content-changed tokens. Both halves are
   load-bearing: without them, the refresh-triggered re-request would itself
   spawn fetches whose unchanged responses fire the next refresh — an
   unbounded request → refresh loop on an idle document. Refresh is sent
   **only when the client advertised
   `workspace.semanticTokens.refreshSupport`** — LSP gates the request behind
   that optional client capability. Without it kakehashi cannot prompt a
   re-pull:
   bridged refinement then surfaces only on the client's own next request
   (typically after an edit), and a passive document stays on its first-paint
   set. This degradation is carried under Consequences.

**Refresh coalescing.** Bridged layers respond independently; firing
`workspace/semanticTokens/refresh` per response would make the client repaint
once per layer — N injections, N visible convergence steps. Refreshes are
therefore **coalesced**: responses landing within a short quiescence window
(after open, after an edit, or after the previous refresh) are batched into a
single refresh; a layer that responds after the window fires its own. The
window length is an implementation concern; the policy — batch within a
window, never block a refresh on the slowest layer — is decided here, because
it fixes how many intermediate paints the user sees. One more property shapes
the policy: `workspace/semanticTokens/refresh` is **parameterless and
workspace-global** — the client re-requests tokens for *every* visible
document, not just the one whose layer responded. Coalescing is therefore
workspace-wide single-flight (concurrent per-document triggers collapse into
one refresh), and one document's bridged refinement does cost re-requests for
unrelated visible documents; those are answered from their current merged
state, so they are cheap, but the fan-out is why the window must not be
per-document.

**Delta baseline contract.** Every array kakehashi returns — the first
response (step 1's synchronously-available set) *and* every subsequent merged
response — is a valid `full/delta`
baseline, tagged with a `resultId`. A `full/delta` request diffs against the
array named by its `previousResultId`, not against an internal "last merged"
cache; if that baseline is no longer retained, kakehashi falls back to a full
response. This keeps the first-response → merged transition expressible as an
ordinary delta, with no special baseline bookkeeping.

**Version contract (owned by the merge).** Each layer's contribution carries
the host document version it was computed against **and the settings
generation in force when it was computed** — the same `token_generation` the
native cache already folds into its key
(`src/lsp/lsp_impl/text_document/semantic_tokens.rs`), because a settings
reload can change bridge-server eligibility, downstream settings, or the
advertised legend without touching the document version. A contribution whose
settings generation is stale is treated exactly like a document-version-stale
one: it is **not source-fresh**, so it never satisfies the no-refetch test
(the layer is re-fetched under the new generation) and is dropped from the
sweep if it cannot be reconciled — this is what stops a settings reload from
being suppressed indefinitely by a contribution that merely matches the
document snapshot. (Edit-shifting still keys on the document version alone;
generation change forces a re-fetch, not a shift.) Acceptance is **monotonic
per layer**, ordered on the pair `(host version, settings generation)`:
in-flight requests can complete out of order, and a response whose
`(version, generation)` is behind the layer's currently accepted contribution
on either component is discarded — a late straggler never replaces newer
tokens with older, edit-shifted ones, and a response from an old generation
can never overwrite state already accepted under a newer generation at the
same document version. The unit all of these contracts operate on is the
**per-server contribution** — tracked as (layer, server, version,
generation) — and a
layer's level-1 stream is derived from its accepted per-server contributions,
so acceptance monotonicity and the material-change refresh test stay
well-defined when a bridge target dispatches to multiple servers (§1). A
bridged set whose version
equals the current host snapshot participates as-is. A **stale** set (version
older than the snapshot) is **not excluded wholesale** — hard exclusion would
downgrade every bridged region to native colors on each keystroke (the client
re-requests after `didChange`, when every bridged set is necessarily stale)
and flip them back when the servers catch up: a per-edit color oscillation
across the whole document, precisely the stale handling editors themselves
avoid by shifting tokens locally. Instead the merge is **stale-tolerant**: the
edit deltas between the set's version and the current snapshot are replayed
over the token positions, shifting each token to its post-edit location. The
trail those deltas come from is **new bookkeeping this decision requires**:
today nothing retains per-version edits — both bridge sync paths send full
text (virt `src/lsp/bridge/text_document/did_change.rs`, host
`src/lsp/bridge/text_document/host.rs`), and the incremental `InputEdit`s
from the client are applied to the tree seed and discarded
(`src/document/model.rs`) — so the implementation must retain the client's
incremental `contentChanges` keyed by host version. The trail stays bounded
because accepted contributions are re-shifted **eagerly on each edit**
(adopting the new version per the identity below), so cached sets never need
history — and an in-flight request whose base version is older than its
server's currently accepted version will have its response discarded on
arrival by monotonic acceptance, so its base need not be covered either.
Retention therefore spans back only to the **minimum currently-accepted
version across active servers**, tighter than the oldest in-flight base. That
minimum alone is not yet bounded: a stuck server whose accepted version never
advances would pin it while edits keep arriving, so the trail is additionally
capped at a **maximum version lag** `L`. When a server's accepted version
falls more than `L` behind the current snapshot, its contribution is
**evicted** — dropped from the sweep (that region falls through to the next
layer, exactly the stale-exclusion fallback) and excluded from the minimum —
so the trail truncates to at most `L` edits regardless of a hung server; a
later fresh response re-enters that server at the current version. This makes
the bound genuinely hang-proof, at the cost of a far-behind server briefly
losing its tokens until it catches up. This cost is carried under
Consequences, not hidden. A token overlapping an edited range cannot be
shifted soundly and is dropped, falling through to the next layer for that
region only. The shifted set participates in the sweep until a fresh set
arrives and fires another refresh. When the trail cannot cover the gap — the
client itself syncs full-text only, the retained window was exceeded, a
version gap after reopen — the set is excluded as the conservative fallback,
so unshifted stale coordinates never reach the sweep. Native is recomputed
synchronously per request and is always current by construction.

**Shift definition.** The shift is an absolute-position mapping obtained by
folding the retained `contentChanges` **in order** through their intermediate
document states — never as independent shifts against the original text,
which misplaces tokens as soon as one batch contains two edits. Each edit
moves same-line positions after it by a character delta, later lines by a
line delta, and combines both for a token on the boundary line of a line
merge or split; a token whose range intersects any edited range is the drop
bucket above. "Intersects" is defined with explicit boundary affinity,
because an insertion has an **empty** replaced range that overlaps nothing
under a naive interval test: an insertion strictly *inside* a token changes
the token's contents and **drops** it; an insertion at the token's start
shifts the whole token right; an insertion at its end leaves the token in
place (the new text falls after it). A non-empty replacement drops any token
whose *interior* it touches and shifts tokens entirely beyond it, under the
same affinity at the shared endpoints. This ordered fold is the "second
exactness obligation" the Consequences carry. A shifted set is thereafter
**treated as computed against the version it was shifted to** *for sweep
participation only*: it satisfies
the participation check for that snapshot while staying **stale by source**,
so it never satisfies the no-refetch test in step 2 above — display currency
and source freshness are distinct states. A later edit re-shifts it from
there over the new deltas
only (a shift of the shifted set), never by re-replaying from the original
version. Tokens dropped by an earlier shift stay dropped until a fresh
response replaces the layer's whole contribution.

This temporal ordering — native now, refined later — is precisely what
`preferred`/`concatenated` (synchronous, one-shot) cannot represent, and why
`merged` is a separate operation rather than a strategy value.

### 3. Legend translation — the advertised slice, user-extensible

kakehashi advertises a legend that is **fixed at `initialize` but composed
from two sources**: the built-in set — `LEGEND_TYPES` / `LEGEND_MODIFIERS`
(`src/analysis/semantic/legend.rs`), exactly the 23 token types and 10
modifiers of the LSP 3.17 standard set (3.18 adds `label` as a 24th standard
type; adopting it is a `legend.rs` change orthogonal to this merge) — plus
**user-declared extra entries** read from config at startup:

```toml
[semanticTokens.extraLegend]
tokenTypes = ["lifetime", "selfKeyword", "builtinType", "formatSpecifier"]
tokenModifiers = ["mutable", "consuming"]
```

Extra entries are appended after the standard ones, so standard indices stay
stable. (The camelCase key names deliberately mirror LSP's
`SemanticTokensLegend` fields, and the table is top-level rather than
per-language because the legend is advertised once per server.) Because config is read before kakehashi answers `initialize`, this
extension is protocol-legal: the legend is still declared exactly once and
never changes afterward — what Alternative D rejects is widening it
*dynamically* per bridged server. Three bounds follow: `tokenModifiers` are
encoded as bit flags in an LSP `uinteger` (0..2^31−1), so the standard 10 plus
extra modifiers must fit 31 bits — config validation **rejects** an
`extraLegend` that would push the total past 31, rather than silently
truncating; the spec asks that a `tokenType` index stay below 65536, so
validation likewise rejects a combined `tokenTypes` list that would exceed
it (a bound stated for completeness — no plausible config approaches it); and
the client's theme must actually
style the extra types — kakehashi passes them through, it cannot make an
editor color a type it has no rule for (Neovim exposes them as
`@lsp.type.<name>` groups; VS Code needs a theme rule or
`semanticTokenColorCustomizations`). A third check runs at `initialize`:
extra entries are compared against what the upstream client announced in
`textDocument.semanticTokens.tokenTypes` / `tokenModifiers` — an entry the
client did not announce is **skipped with a warning** rather than advertised,
since the client never declared it renderable and tokens carrying it would
name a type the client has no notion of.

**Downstream capability advertisement.** Today the bridge handshake
advertises **no** `textDocument.semanticTokens` client capability at all
(`src/lsp/bridge/protocol/client_capabilities.rs`): kakehashi has not
announced token support as a client, so it must not issue semantic-token
requests yet — and servers commonly gate `semanticTokensProvider` on the
announced client capability, leaving the merge nothing to fetch. Token
bridging must extend the baseline: advertise
`requests` `{full, full.delta, range}`, `formats = ["relative"]`, and
`tokenTypes`/`tokenModifiers` mirroring kakehashi's own advertised legend —
while continuing to advertise neither `overlappingTokenSupport` (level 1's
"non-overlapping by protocol" relies on its absence) nor
`multilineTokenSupport` (bridged multiline tokens are split during
normalization regardless). This is part of Strategy 1's plumbing, listed here
because the legend contract depends on it.

Bridged tokens are decoded with the server's own legend, then re-encoded into
kakehashi's advertised legend:

- Token types/modifiers present in the advertised legend — standard or extra —
  map **1:1, losslessly**.
- A token whose **type** is outside the advertised legend has **no slot** and
  is **dropped** (with a debug log), not coerced. There
  is deliberately **no fuzzy "nearest standard type" matching** — guessing that
  rust-analyzer's `selfKeyword` is "really" `keyword` would mislabel as often
  as it helps, and dropping does not create a hole: the level-2 sweep falls
  through to the next layer in `priorities` (ultimately native tree-sitter),
  which may still highlight that character when native tokenized it; otherwise
  it stays untokenized, matching native-only behavior. Because the legend is
  frozen at `initialize` and bridged servers connect afterward, kakehashi
  **cannot** widen the legend at that point — the remedy is the user-declared
  `extraLegend` route above, applied *before* the freeze.
- **Modifiers** are handled independently of the type: a token whose base type
  *is* standard is emitted even if it carries non-standard modifiers; the
  unknown modifiers are simply dropped (mirroring the existing "unknown
  modifiers are silently ignored" behavior of
  `map_capture_to_token_type_and_modifiers`, `resolve_capture`'s delegate in
  `legend.rs`). Only an
  unmappable *base type* drops the whole token.
- **Dropping is loud, not silent.** When a bridged server's `initialize`
  result arrives, its legend is diffed against kakehashi's advertised one:
  types and modifiers with no slot trigger **one `window/showMessage` warning
  per server per session**, naming the server and the unmapped entries and
  pointing at `semanticTokens.extraLegend` (the full list also goes to
  `window/logMessage`). The user learns *at connect time* what to register to
  stop the drops — silent narrowing was the discoverability hole in the drop
  rule, and the warning closes it without any fuzzy matching.
- **Standard-typed bridged tokens are trusted wholesale.** When a server labels
  a character with a standard type kakehashi would classify differently, the
  *server's* label wins (it is the higher-ranked layer) — this is intentional,
  not a mislabel to guard against. Preferring a bridged layer *is* a statement
  that its classification is more authoritative than tree-sitter's for that
  region (rust-analyzer resolving a `type` where tree-sitter only sees a
  `variable` is the whole point). kakehashi does not second-guess a standard
  type from a server it was configured to bridge. A per-server type
  allow/drop/remap policy (for a server whose standard-type usage a user
  distrusts) is a possible future extension — the existing capture-mapping
  machinery is the natural home — but is **out of scope** here.

**Honest consequence**: out of the box, cross-layer tokens deliver the
*standard slice* of a server's output, not its full richness. The motivating
server, rust-analyzer, expresses much of its type-awareness through ~30
**custom** types (`lifetime`, `selfKeyword`, `builtinType`, `formatSpecifier`,
…) that the default legend drops (falling through to native) — and dropping is
not only a richness loss but a *consistency* one: the same construct can
render in the server's color where it used a standard type and in
tree-sitter's where it did not, a per-character patchwork. The ceiling is
user-liftable — registering those types in `semanticTokens.extraLegend` passes
them through losslessly, and the connect-time warning names exactly what to
register — but lifting it costs a maintained list plus client-theme rules for
the extra types. Automatic widening or post-connect renegotiation stays
impossible (see Alternative D).

### `merged` representation

Resolving the open cost cross-layer-aggregation originally flagged — in its
pre-revision wording, "a stage-2-only `merged` would force either enum growth
visible to stage 1 or a type split"; its Neutral bullet now defers here:

- `merged` is **not** added to `AggregationStrategy`. It is not a value any
  user writes anywhere. Other stage-2 methods are untouched, and stage 1
  (`bridge.<key>.aggregation`) keeps its enum unchanged — for semantic-tokens
  methods its `priorities` is reused as the intra-layer server rank (§1) and a
  written `strategy` is rejected the same way as at stage 2; no enum grows, no
  type splits.
- Semantic-tokens methods dispatch to the merge path **internally**, the way
  formatting already gets a per-method default strategy
  (`default_layer_strategy_for_method`). The user still controls *which* layers
  participate and their *rank* via
  `layers.aggregation."textDocument/semanticTokens/full".priorities`.
- A `strategy` written on a semantic-tokens method — at stage 2
  (`layers.aggregation`) or stage 1 (`bridge.<key>.aggregation`) — (e.g.
  `strategy = "preferred"`) **must be rejected at config validation** with a
  message pointing here — rather than silently ignored — because honoring it
  would reintroduce the gap-leaving wholesale-winner behavior this decision
  rejects. This validation does not exist yet: today `LayerAggregationConfig`
  carries an `Option<AggregationStrategy>` that `resolve_layers` applies to any
  method without semantic-token special-casing (`src/config/settings.rs`).
  Adding the rejection (or, equivalently, ignoring `strategy` for these methods
  with a warning) is new surface this decision puts on the implementer; no
  current behavior is implied.

### Semantics

- **`priorities` ranks layers; the sweep makes them per-region winners.** This
  is the same ordered-allowlist field cross-layer-aggregation defined — a layer
  omitted from `priorities` does not participate; `["native"]` reproduces
  today's output exactly.
- **`enabled` still gates.** Host participates only when
  `bridge._self.enabled = true`; each virt injection only when
  `bridge.<inj>.enabled`. A disabled or silent layer is an empty contributor
  the sweep skips — identical to cross-layer-aggregation.
- **Native is the floor among the layers.** Listed last by default, it claims
  every region it tokenizes that no higher-priority layer claims, so wholesale
  layer selection adds no new gaps relative to native-only output. It does not
  make the output dense — characters no layer tokenizes stay untokenized, as
  they are today.
- **`semanticTokens/range` delegates to `full`.** As today
  (semantic-token-overlap-resolution), the range handler reuses the full
  pipeline, so the spatial merge applies without separate work. Temporally it
  is also native-first: a range request returns the requested slice of the
  current document-versioned merged state (native-only at first), starts the
  same bridged fetches, and surfaces bridged refinement through the same
  workspace-global `workspace/semanticTokens/refresh` — there is no
  range-local refresh and no `range/delta` (the LSP protocol defines no delta for range),
  so range responses are always full slices, never deltas.

### Out of Scope

- **`overlappingTokenSupport` stacking.** When the client declares
  `overlappingTokenSupport: true`, single-winner-per-region is no longer
  forced — layers could be *stacked* (e.g. native `markup.raw` background under
  bridged type tokens) instead of resolved to one winner. Whether to skip the
  level-2 sweep in that case is the same question
  semantic-token-overlap-resolution Phase 4 already opened for the intra-layer
  sweep; it is decided there, not here. This decision specifies the
  universally-correct non-overlapping path only.
- **Automatic legend widening.** §3's `extraLegend` admits custom types the
  user declares up front; *discovering* a bridged server's legend at runtime
  and widening kakehashi's to match would mean renegotiating an
  already-initialized legend — impossible under static registration, and
  reachable only through dynamic re-registration, whose optional client
  support cannot carry a default behavior (Alternative D). The
  connect-time warning is the designed substitute: it tells the user what to
  declare, it never declares it for them.
- **Re-fetching/recomputing stale bridged sets** — *the mechanism*, not the
  contract. The version contract the merge enforces is in scope and stated in
  §2 (every layer contribution carries the host version it was computed
  against; a stale set is edit-shifted where the edit trail allows, excluded
  where it does not, until a fresh set arrives). What is deferred to bridge
  infrastructure is *how* a stale virt set is recomputed and *when* the
  re-fetch happens after an edit — the same edit-invalidation every bridged
  response already relies on (language-server-bridge-virtual-document-model
  syncs virtual docs on
  host change). The bridge does **not** cancel its own in-flight downstream
  requests on edit — it only forwards an upstream `$/cancelRequest` when one
  arrives — so late responses from superseded requests are expected, and are
  handled by §2's monotonic acceptance rather than assumed away by
  cancellation. The sweep never sees
  unshifted stale coordinates: a set is either shifted to the current snapshot
  or excluded.
- **Per-injection-language rank relative to host.** Inherited verbatim from
  cross-layer-aggregation's Out of Scope: `priorities` ranks the three layer
  kinds, not individual injection languages.

## Consequences

### Positive

- **Closes cross-layer-aggregation's deferred question** with a concrete design
  that reuses its `priorities` field unchanged; the only new config surface is
  the optional `semanticTokens.extraLegend` table (§3).
- **Instant highlighting preserved.** Native-first temporal refinement keeps
  the zero-latency tree-sitter experience; bridged richness arrives as a
  refresh where the client supports it, never blocking the first paint.
- **Stable colors while typing.** The stale-tolerant version contract (§2)
  keeps bridged tokens on screen across edits by shifting them, instead of
  reverting the document to native colors on every keystroke and flipping back
  after each bridged response. Contingent on the edit-delta trail §2 requires:
  clients syncing full-text only fall back to exclusion and keep the
  oscillation.
- **The legend ceiling is user-liftable.** `semanticTokens.extraLegend` plus
  the connect-time warning turn dropped custom types from a silent hard bound
  into a discoverable, opt-in extension — without fuzzy matching or protocol
  violations.
- **Thin new surface.** The genuinely new code is the level-2 sweep (a
  re-parameterization of the existing one), the pre-level-2 virt collapse
  (another application of the same deepest-first sweep, over per-injection virt
  streams), the legend re-encode table, and the version-keyed edit-delta trail
  §2's stale shift requires (a bounded per-document log of incremental
  `contentChanges` plus the replay over token positions — the one piece with
  no existing counterpart). Native's intra-layer finalize, coordinate remap,
  and layer-priority resolution are all reused.
- **Stage 1 and `AggregationStrategy` untouched.** Keeping `merged` off the
  enum means no typo-surface or namespace growth for the 30+ other methods.
- **Allowlist suppression for free.** `["native"]` (today's behavior) and
  `["host", "native"]` (no injection servers) are spellable without a dedicated
  switch.

### Negative

- **Bounded-by-default richness.** Out of the box the headline server
  (rust-analyzer) loses ~30 custom types to the drop, with the per-character
  patchwork §3 describes. Lifting the bound is manual: the user maintains the
  `extraLegend` list, and their editor theme must style the extra types.
- **Bridged richness rides on `refreshSupport`.** Refresh is the only path
  that delivers bridged tokens to an idle document; a client that does not
  advertise `workspace.semanticTokens.refreshSupport` re-requests only on its
  own schedule (typically the next edit), so between requests it effectively
  sees native-only output. Instant paint and correctness are unaffected —
  only the arrival of bridged refinement is.
- **Refresh coalescing is load-bearing.** §2's quiescence-window policy bounds
  how many intermediate paints the user sees, but the window length is a
  tuning knob: too short re-admits per-layer thrash, too long delays bridged
  richness.
- **Temporal flicker at open.** Tokens visibly change color shortly after open
  as bridged results first land. Inherent to progressive refinement
  (language-server-bridge-request-strategies Strategy 1); the alternative (block on all layers) is
  rejected below. The stale-tolerant contract confines this to first
  convergence — steady-state typing no longer oscillates.
- **Two-coordinate bookkeeping, plus edit replay.** virt→host remap must be
  exact, or merged tokens land on the wrong characters — a sharper failure
  mode than for position requests, where one wrong location is visible in
  isolation. The stale-shift contract adds a second exactness obligation: the
  **ordered** replay of edit deltas over token positions (§2's fold — never
  independent shifts against the original text). Injection-fence boundaries —
  where clipped/split bridged fragments meet native tokens — are the most
  visible artifact zone and the natural focus for merge tests.

### Neutral

- **Native keeps its internal sweep unchanged.** Native's `finalize_tokens` is
  untouched; level 1 adds only the virt-collapse step (for layers with multiple
  injection streams), and this decision adds the inter-layer level strictly
  above. The levels are independently testable.
- **`merged` is invisible in config.** Users never type it; it surfaces only as
  "semantic tokens compose all enabled, listed layers." Documentation, not a
  schema value, carries the concept.

## Alternatives Considered

### A. Fold semantic tokens into `concatenated`

Treat tokens like diagnostics: append every layer's tokens in `priorities`
order.

**Rejected because**: tokens are a spatial tiling, not an item list. Appending
produces overlapping, out-of-order tokens — an invalid delta stream. The whole
reason cross-layer-aggregation scoped tokens out.

### B. One-shot merge — block until all layers respond

Await native, host, and virt, run the sweep once, return the final set.

**Rejected because**: it destroys instant highlighting. Bridged servers can
take seconds on first request; the user would stare at an unhighlighted buffer.
language-server-bridge-request-strategies Strategy 1 exists precisely to avoid this; `merged` adopts
its temporal shape.

### C. Always return layered/overlapping tokens, let the client merge

Emit each layer's tokens raw and rely on `overlappingTokenSupport`.

**Rejected because**: identical to semantic-token-overlap-resolution
Alternative 2 — not universally supported, pushes layering into every editor,
and `textDocument/semanticTokens` has no layered API. Retained only as an
*opportunistic* path when the client opts in (Out of Scope, deferred to
overlap-resolution Phase 4), never as the default.

### D. Advertise a per-server / dynamic superset legend

Widen kakehashi's legend to union every bridged server's custom types so
nothing is dropped.

**Rejected because**: kakehashi's legend is bound at `initialize` (static
registration), and bridged servers are spawned only *afterward* — on injection
detection (language-server-bridge) — so their legends are unknowable when
kakehashi must declare its own. The protocol's only post-initialize path is
re-registering the capability via `client/registerCapability` with a new
legend, which works only when the client advertises `dynamicRegistration` for
semantic tokens — optional support that cannot carry a default behavior (and
each re-registration would invalidate every outstanding `resultId` baseline).
What survives from this alternative is its
*static* half, adopted in §3: entries the **user** declares in
`semanticTokens.extraLegend` are known from config before `initialize`, so
they extend the legend without renegotiation, and the connect-time warning
tells the user what a server offered that the declared legend lacks. What
stays rejected is the automatic union — kakehashi never grows the legend on
its own.

### E. Make `merged` a user-selectable `AggregationStrategy`

Add `Merged` to the enum so users can write `strategy = "merged"` like
`preferred`/`concatenated`.

**Rejected because**: it is meaningful for only two methods and needs temporal
legend machinery no other method has. Exposing it enum-wide invites
`strategy = "merged"` on `hover` (meaningless) and forces the stage-1/stage-2
type split cross-layer-aggregation deliberately deferred. The
per-method internal default (formatting's precedent) gives users the real knob
— layer rank via `priorities` — without polluting the strategy namespace.

### F. Layer rank as the least-significant sweep key

Keep native's `(priority, depth, inverse node_byte_len, pattern_index)` key
(`token_priority` in `src/analysis/semantic/finalize.rs`) dominant and use
layer rank only to break remaining ties.

**Rejected because**: it inverts the intent. A native tree-sitter token on a
deeply-nested node would outrank a richer bridged token covering the same
characters, so the bridged layer — the entire point of the merge — would rarely
win. Layer rank must be the *primary* key for a higher-ranked layer to take a
region; intra-layer specificity only matters within a layer, which level-1
finalize has already resolved.
