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
not bridged (request-strategies Strategy 1 is marked ❌ Not implemented), and
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
ordering, and "a future `merged`-style strategy may bring them in." This is
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
  request that blocks on all layers (request-strategies Strategy 1).
- **One advertised legend, fixed at `initialize`.** LSP fixes the
  `SemanticTokensLegend` at server `initialize`. Bridged servers are spawned
  **only after** kakehashi has already initialized — on injection detection
  during parsing of an already-open document (language-server-bridge;
  ls-bridge-async-connection handles the async I/O) — so their legends are
  unknown when kakehashi must declare its own. The merge cannot widen the
  legend per server.
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

```
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
  stream.** Native is the existing `finalize_tokens` output. Host is one
  server's response, non-overlapping by protocol. **Virt is not a single
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
suppresses it for the
method (allowlist semantics, identical to cross-layer-aggregation) — e.g.
`["host", "native"]` ignores injection servers; `["native"]` is exactly
today's behavior.

### 2. Temporal refinement (progressive)

The merge is not one-shot. It adopts request-strategies Strategy 1's
parallel-fetch-with-progressive-refinement shape, but makes one choice Strategy
1 leaves open: where Strategy 1 permits using a bridged response directly if it
wins the race, this merge is **deliberately native-first** — the first response
never waits on a race outcome. Native tokens are local and effectively always
ready first; tying the instant-paint guarantee to a race would make the
zero-latency experience nondeterministic for no benefit. (Native-first
presumes `native` participates, the default; the qualification below covers a
config that suppresses it.)

1. The first response to a `semanticTokens/full` request returns the
   **synchronously-available set immediately** — the native set when `native`
   is in `priorities` (the default), bridged layers fetched in parallel but not
   awaited even if one could return first. If `priorities` **omits** `native`
   (e.g. `["host"]`), there is no instant local layer: the first response is
   whatever bridged set is already cached for the current version, or an empty
   result if none is — and the bridged layers still refine via step 2. Omitting
   native therefore trades instant highlighting for bridged-only output; the
   default keeps the instant-paint guarantee.
2. When a bridged layer responds, kakehashi recomputes the level-2 sweep and
   emits `workspace/semanticTokens/refresh`; the client re-requests and
   receives the merged set (as a `full/delta` where the client supports it).

**Delta baseline contract.** Every array kakehashi returns — the first response
(native-only by default, or a cached/empty bridged set when `native` is
omitted) *and* every subsequent merged response — is a valid `full/delta`
baseline, tagged with a `resultId`. A `full/delta` request diffs against the
array named by its `previousResultId`, not against an internal "last merged"
cache; if that baseline is no longer retained, kakehashi falls back to a full
response. This keeps the first-response → merged transition expressible as an
ordinary delta, with no special baseline bookkeeping.

**Version contract (owned by the merge).** Each layer's contribution carries
the host document version it was computed against. The level-2 sweep includes a
bridged set **only if its version equals the current host snapshot**; a set
that predates the snapshot — e.g. a bridged response that landed, triggered a
refresh, and was then outdated by a host edit before the client re-requested —
is excluded, and that region falls through to the next layer in `priorities`
(ultimately the always-current native set when `native` participates; otherwise
the region is simply untokenized) until a fresh bridged set arrives and fires
another refresh. This is what makes deferring the *recompute mechanism* (Out of Scope)
safe: a stale set never enters the sweep, so the merge never lands tokens on
shifted coordinates. Native is recomputed synchronously per request and is
always current by construction.

This temporal ordering — native now, refined later — is precisely what
`preferred`/`concatenated` (synchronous, one-shot) cannot represent, and why
`merged` is a separate operation rather than a strategy value.

### 3. Legend translation — bounded to the standard slice

kakehashi advertises a **fixed** legend: `LEGEND_TYPES` /`LEGEND_MODIFIERS`
(`src/analysis/semantic/legend.rs`) are **exactly** the standard LSP set (23
types, 10 modifiers) — not a superset. Bridged tokens are decoded with the
server's own legend, then re-encoded into kakehashi's:

- Token types/modifiers that **are** standard map **1:1, losslessly**.
- A token whose **type** is outside the standard set has **no slot** in the
  advertised legend and is **dropped** (with a debug log), not coerced. There
  is deliberately **no fuzzy "nearest standard type" matching** — guessing that
  rust-analyzer's `selfKeyword` is "really" `keyword` would mislabel as often
  as it helps, and dropping does not create a hole: the level-2 sweep falls
  through to the next layer in `priorities` (ultimately native tree-sitter),
  which may still highlight that character when native tokenized it; otherwise
  it stays untokenized, matching native-only behavior. Because the legend is
  frozen at
  `initialize` and bridged servers connect afterward, kakehashi **cannot**
  widen the legend to admit the custom type instead.
- **Modifiers** are handled independently of the type: a token whose base type
  *is* standard is emitted even if it carries non-standard modifiers; the
  unknown modifiers are simply dropped (mirroring `resolve_capture`'s existing
  "unknown modifiers are silently ignored" behavior in `legend.rs`). Only an
  unmappable *base type* drops the whole token.
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

**Honest consequence**: cross-layer tokens deliver the *standard slice* of a
server's output, not its full richness. The motivating server, rust-analyzer,
expresses much of its type-awareness through ~30 **custom** types
(`lifetime`, `selfKeyword`, `builtinType`, `formatSpecifier`, …) that this
merge drops (falling through to native). The win is real but bounded — richer
*standard-typed*
tokens (e.g. resolved `type` vs. tree-sitter's `variable`), not the server's
complete legend. The ADR states this plainly so the merge is not mistaken for
lossless pass-through; advertising a wider non-standard legend, or
renegotiating it after the bridge connects, is **out of scope** (see
Alternative D).

### `merged` representation

Resolving the open cost cross-layer-aggregation flagged ("a stage-2-only
`merged` would force either enum growth visible to stage 1 or a type split"):

- `merged` is **not** added to `AggregationStrategy`. It is not a value any
  user writes anywhere. Stage 1 (`bridge.<key>.aggregation`) and other stage-2
  methods are untouched; no enum grows, no type splits.
- Semantic-tokens methods dispatch to the merge path **internally**, the way
  formatting already gets a per-method default strategy
  (`default_layer_strategy_for_method`). The user still controls *which* layers
  participate and their *rank* via
  `layers.aggregation."textDocument/semanticTokens/full".priorities`.
- A `strategy` written on a semantic-tokens method (e.g.
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
  document-level `workspace/semanticTokens/refresh` — there is no range-local
  refresh and no `range/delta` (the LSP protocol defines no delta for range),
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
- **Custom (non-standard) bridged token types.** Admitting them would require a
  wider advertised legend negotiated *after* the bridge connects, which LSP
  does not allow against an already-initialized legend (Alternative D).
- **Re-fetching/recomputing stale bridged sets** — *the mechanism*, not the
  contract. The version contract the merge enforces is in scope and stated in
  §2 (every layer contribution carries the host version it was computed
  against; the merge excludes any set whose version ≠ the current snapshot,
  falling back to the next layer for that region until a fresh set arrives).
  What is deferred to bridge infrastructure is *how* a stale virt set is
  recomputed and *when* the refresh fires after an edit — the same
  edit-invalidation every bridged response already relies on
  (virtual-document-model syncs virtual docs on host change; old in-flight
  requests are cancelled). The merge never needs to repair stale coordinates
  because the contract forbids a stale set from entering the sweep in the first
  place.
- **Per-injection-language rank relative to host.** Inherited verbatim from
  cross-layer-aggregation's Out of Scope: `priorities` ranks the three layer
  kinds, not individual injection languages.

## Consequences

### Positive

- **Closes cross-layer-aggregation's deferred question** with a concrete design
  that reuses its `priorities` field unchanged — no new config surface.
- **Instant highlighting preserved.** Native-first temporal refinement keeps
  the zero-latency tree-sitter experience; bridged richness arrives as a
  refresh, never blocking the first paint.
- **Thin new surface.** The genuinely new code is the level-2 sweep (a
  re-parameterization of the existing one), the pre-level-2 virt collapse
  (another application of the same deepest-first sweep, over per-injection virt
  streams), and the legend re-encode table. Native's intra-layer finalize,
  coordinate remap, and layer-priority resolution are all reused.
- **Stage 1 and `AggregationStrategy` untouched.** Keeping `merged` off the
  enum means no typo-surface or namespace growth for the 30+ other methods.
- **Allowlist suppression for free.** `["native"]` (today's behavior) and
  `["host", "native"]` (no injection servers) are spellable without a dedicated
  switch.

### Negative

- **Bounded, not lossless, richness.** The standard-slice legend constraint
  means the headline server (rust-analyzer) loses ~30 custom types to
  the drop. The feature's value is real but smaller than "full
  server-quality tokens" — this is half the answer to "is the complexity worth
  it," and is stated rather than hidden.
- **Refresh thrash.** Each bridged layer responding independently triggers a
  recompute + `workspace/semanticTokens/refresh`. N bridged injections in one
  document can mean N refreshes unless responses within a window are debounced
  before refreshing — debouncing is an implementation concern flagged here, not
  designed.
- **Temporal flicker.** Tokens visibly change color shortly after open as
  bridged results land. Inherent to progressive refinement (request-strategies
  Strategy 1); the alternative (block on all layers) is rejected below.
- **Two-coordinate bookkeeping.** virt→host remap must be exact, or merged
  tokens land on the wrong characters — a sharper failure mode than for
  position requests, where one wrong location is visible in isolation.

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
request-strategies Strategy 1 exists precisely to avoid this; `merged` adopts
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

**Rejected because**: LSP fixes the legend at `initialize`, and bridged servers
are spawned only *afterward* — on injection detection (language-server-bridge) —
so their legends are unknowable when kakehashi must declare its own. There is no protocol-legal way
to grow the legend post-initialize. The fixed standard set is the only
legend available at the binding moment; the standard-slice bound is a
consequence of LSP, not a kakehashi choice.

### E. Make `merged` a user-selectable `AggregationStrategy`

Add `Merged` to the enum so users can write `strategy = "merged"` like
`preferred`/`concatenated`.

**Rejected because**: it is meaningful for only two methods and needs temporal
+ legend machinery no other method has. Exposing it enum-wide invites
`strategy = "merged"` on `hover` (meaningless) and forces the stage-1/stage-2
type split cross-layer-aggregation called an "acceptable deferred cost." The
per-method internal default (formatting's precedent) gives users the real knob
— layer rank via `priorities` — without polluting the strategy namespace.

### F. Layer rank as the least-significant sweep key

Keep native's `(depth, node_depth, pattern_index)` dominant and use layer rank
only to break remaining ties.

**Rejected because**: it inverts the intent. A native tree-sitter token on a
deeply-nested node would outrank a richer bridged token covering the same
characters, so the bridged layer — the entire point of the merge — would rarely
win. Layer rank must be the *primary* key for a higher-ranked layer to take a
region; intra-layer specificity only matters within a layer, which level-1
finalize has already resolved.
