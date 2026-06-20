# Cross-Layer Result Aggregation

**Related Decisions**:
- [host-document-bridge](host-document-bridge.md) — Host bridging schema (`bridge._self`); declared cross-layer combine logic out of scope, which this decision now covers
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md) — The unified ordered-allowlist semantics that the layer `priorities` follows
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method strategies and multi-server merging *within* a bridge target
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model (virt bridges)
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — Wildcard merge machinery reused for the method-keyed map

## Implementation Status

Phased roadmap:

1. **Allowlist `priorities` with `"*"`** (aggregation-priorities-wildcard) —
   ✅ implemented (`src/lsp/aggregation/server/priority.rs`).
2. **Layer dispatch, `preferred` only** — ✅ implemented: the
   `LanguageSettings.layers` schema, `resolve_layers` (wildcard merge), and
   a virt-layer gate in every bridge entry point — request handlers via
   `Kakehashi::virt_layer_enabled`, the push-diagnostics scheduler via
   `resolve_layer_config_from_settings` directly (it already holds a loaded
   settings arc), keyed `textDocument/publishDiagnostics` to match its
   aggregation config. With host bridging (host-document-bridge)
   implemented for every bridged request method, handlers run the real
   stage-2 `preferred` walk (`Kakehashi::walk_layers` →
   `race_layers_preferred`): the virt and host layers fan out
   **concurrently** — the layer-level analogue of the stage-1 `preferred`
   fan-in — and the highest-priority non-empty result wins. A buffered
   lower-priority result waits on a still-pending higher layer (priority
   decides, not arrival), the losing in-flight future is dropped
   best-effort, and layers absent from `priorities` are never polled. The
   latency cost of a layer answering empty is therefore `max`, not `sum`;
   the trade-off is downstream load — the host server is queried even when
   virt ends up winning, mirroring how stage-1 queries every server of a
   target. Virt answers inside injections, host answers on the host
   document when `bridge._self.enabled = true`.
3. **Layer-level `concatenated` for `textDocument/formatting` only** — ✅
   implemented as a true sequential cross-layer pipeline in
   `formatting_impl`: layers run in priority order, each formatting the previous
   layer's output (virt region edits are applied to the host text, the host
   formatter then formats that intermediate text). With one producing layer
   the minimal edits pass through verbatim (they are against the original by
   construction); with two or more the chain collapses into a single
   whole-document replacement edit — the same overlap-free shape as the
   within-region pipeline (concatenated-formatting-pipeline Decision
   point 4), and `None` when the chain round-trips to the original.
   Constraint: virt edits resolve against the parsed snapshot, so virt can
   only run while the accumulated text still is the snapshot text — a
   `priorities` placing virt after a producing layer skips virt with a warning.
   The default order (virt before host) is unaffected. Under `preferred`
   the walk stays lazy: the first layer producing edits wins and later
   layers are never contacted. Layer strategy applies to full formatting
   only; `textDocument/rangeFormatting` stays on `preferred`, mirroring the
   stage-1 rule.
4. **Layer-level `concatenated` for diagnostics** — ✅ implemented for both
   pull (`textDocument/diagnostic`) and synthetic push
   (`textDocument/publishDiagnostics`): virt and host gate independently on
   `priorities` membership (host additionally on `bridge._self.enabled`),
   both layers fan out concurrently, and `combine_layer_diagnostics`
   applies the resolved strategy — `concatenated` (the default) appends
   items in `priorities` order, `preferred` returns the first non-empty
   layer. The host layer is a `textDocument/diagnostic` pull with the real
   URI — push-propagation-diagnostic-forwarding plans to drive host diagnostics by
   push too, but that is not yet implemented — combined within the layer per
   `bridge._self.aggregation` via `dispatch_host_concatenated` /
   `dispatch_host_preferred`. Diagnostics are joined rather than raced:
   they are not latency-interactive and `concatenated` needs every layer
   anyway, so one code path with a pure combine function replaces the
   `race_layers_preferred` machinery here.

## Context

Once host bridging exists, a single LSP request to kakehashi can be answered
by up to three **result layers**:

1. **native** — kakehashi's own features (Tree-sitter semantic tokens,
   locals-based definition, document symbols, folding, …)
2. **host** — the `bridge._self` route: responses from host-language servers
   (e.g., marksman on the Markdown document), already aggregated across
   servers by `bridge._self.aggregation`
3. **virt** — the `bridge.<inj>` route: responses from injection-language
   servers (e.g., pyright on a Python code block), already aggregated across
   servers by `bridge.<inj>.aggregation`

host-document-bridge deliberately scoped out "how responses from both roles
are ordered, merged, or routed per method." This decision fills that gap:
users need at minimum to reorder the three layers per method, and ideally to
choose a combine strategy (first-non-empty vs. merge).

### Why the existing `priorities` field cannot express this

`AggregationConfig.priorities` is a `Vec<String>` of **language server names**
— an open, user-defined namespace scoped to a single bridge target.
Layers are a different axis: a **closed set of exactly three kinds**, sitting
*above* the per-target aggregation. Mixing layer identifiers into a
server-name list would conflate two namespaces in one field — the same
violation that led host-document-bridge to reject its Alternative A
(role-tagged priority entries).

## Decision Drivers

- **Namespace separation**: server names (open set) and layers (closed set)
  must not share a field or a value space.
- **Two-stage aggregation**: per-target server aggregation (stage 1, exists
  today) and cross-layer aggregation (stage 2, this decision) compose; neither
  reaches into the other.
- **Reuse wildcard machinery**: the method-keyed map should resolve via
  `resolve_with_wildcard`, exactly like `bridge.<key>.aggregation`.
- **Defaults preserve current behavior**: with no user config, requests behave
  as they do today (virt preferred where an injection exists, native
  otherwise; host inert until opted in).
- **One list semantics**: the layer list follows the same ordered-allowlist
  rule as the stage-1 server list (aggregation-priorities-wildcard), differing
  only in element type. Both fields are therefore named `priorities` — one
  name for one semantics — with the element type (closed `LayerSource` enum
  vs. open server-name strings) carrying the axis distinction.

## Decision Outcome

**Chosen approach**: add a `layers` field to `LanguageSettings` whose
`aggregation` member is a method-keyed map (LSP method name or `_`
wildcard) of closed-enum layer orders plus an optional strategy. The
nesting mirrors `bridge.<key>.aggregation` — "aggregation" uniformly names
the method-keyed map across the config — and keeps headroom for future
layer-wide fields on `layers` without colliding with method names.
Stage-1 types (`AggregationConfig`, `BridgeLanguageConfig`) are untouched.

### Schema

```toml
# ---- Built-in defaults (declared in code; not user-facing) ----
[languages._.layers.aggregation._]
priorities = ["virt", "host", "native"]   # innermost-first; mirrors "deeper wins"
# host is listed but contributes nothing until the user opts in via
# bridge._self.enabled (host-document-bridge) — priorities ranks layers,
# enabled flags decide whether a bridge target exists at all.
# strategy: per-method default (concatenated for diagnostics, else preferred)

# ---- User: markdown hover should prefer the host LS, and drop native ----
[languages.markdown.layers.aggregation."textDocument/hover"]
priorities = ["host", "virt"]             # allowlist: native does not run

# ---- Stage 1 stays exactly as before: server names within one target ----
[languages.markdown.bridge._self.aggregation."textDocument/hover"]
priorities = ["marksman"]

[languages.markdown.bridge.python.aggregation._]
priorities = ["pyright"]
```

```rust
/// Result layers: kakehashi native, host bridge, virt bridges.
/// NOT injection nesting depth (tree-sitter "language layers").
#[serde(rename_all = "lowercase")]
pub enum LayerSource {
    Native, // kakehashi's own features
    Host,   // bridge._self aggregate
    Virt,   // bridge.<inj> aggregate at the request position/document
}

pub struct LayerAggregationConfig {
    /// Ordered allowlist, highest first: layers omitted from the list do
    /// not participate for that method. The set is closed and three-valued,
    /// so explicit enumeration replaces the `"*"` element used by the
    /// stage-1 server `priorities` — "the rest" is always spellable by name.
    pub priorities: Option<Vec<LayerSource>>,
    /// Reuses the stage-1 strategy type; omit for the per-method default.
    pub strategy: Option<AggregationStrategy>,
}

pub struct LayersConfig {
    /// Per-method map, keyed by LSP method name or "_" (wildcard).
    pub aggregation: Option<HashMap<String, LayerAggregationConfig>>,
}

// On LanguageSettings, beside `bridge`:
pub layers: Option<LayersConfig>,
```

### Two-Stage Aggregation

```
Stage 2 (this decision)   layers.aggregation.<m>.priorities = [virt, host, native]   ← LayerSource enum
                             │      │      └ native handler result
                             │      └ bridge._self.aggregation.priorities  ← LS names
                             └ bridge.<inj>.aggregation.priorities         ← LS names
Stage 1 (exists today)    each layer resolves its own servers into
                          one response per layer before stage 2 runs
```

Each stage owns one namespace. Stage 1 orders **server names** within a
bridge target; stage 2 orders **layers**. Both lists follow the
ordered-allowlist rule of aggregation-priorities-wildcard. The
`AggregationStrategy` *type* is shared (the "how to combine multiple
responses" semantics are identical), but the configured *values* are
independent — e.g., diagnostics can be `concatenated` across layers while
`bridge.python.aggregation` keeps `preferred` among Python servers.

### Semantics

- **`priorities` is an ordered allowlist.** Layers omitted from the list do
  not participate for that method — `priorities = ["virt", "host"]`
  suppresses native; `priorities = []` disables the method across all layers
  (mirroring the stage-1 `priorities = []`). No `"*"` element is supported:
  with a closed three-value set, "the rest" is always expressible by explicit
  enumeration, so the enum stays pure.
- **`priorities` ranks; `enabled` gates.** Host participation is additionally
  gated by `bridge._self.enabled` (host-document-bridge), and per-injection
  virt participation by `bridge.<inj>.enabled`. These are not a double
  opt-in: the default `priorities` already lists host, so enabling
  `bridge._self.enabled = true` is the *only* step a user takes to bring the
  host layer in. A disabled target is simply an empty contributor.
- **Empty contributors are skipped.** A layer with nothing to say — no native
  implementation for the method, host bridging disabled, no injection at the
  request position — is an empty contributor; `preferred` falls through to
  the next layer.
- **Resolution reuses wildcard machinery.** The method key resolves via
  `resolve_with_wildcard` (method-specific entry inherits unset fields from
  `_`), and the `layers` field participates in the outer language-level
  wildcard/base merge like `bridge` does. Note `priorities` is a single
  field: a method-specific `priorities` replaces the wildcard's list
  wholesale, it does not merge element-wise.
- **Strategy is phased.** Phase 2 implements `preferred` only; with
  layer-level `concatenated` landed for formatting (phase 3) and
  diagnostics (phase 4), the layer defaults come from
  `default_layer_strategy_for_method`: `concatenated` for diagnostics
  *and* `textDocument/formatting`, `preferred` otherwise. The formatting
  default diverges from the bridge level deliberately — the cross-layer
  pipeline composes disjoint work (virt formats injection regions, host
  formats the resulting text), whereas multiple servers of one bridge
  target produce competing whole-document edits, so the bridge default
  stays `preferred`. Host is opt-in, so the defaults still reproduce
  single-layer behavior until `bridge._self.enabled = true`.
- **Nested injections stay implicit.** When injections nest
  (markdown → python → sql), the virt layer resolves deepest-first,
  consistent with the semantic-token priority convention (deeper wins). Depth
  order is not user-configurable.

### Out of Scope

- **Per-virt-language ordering relative to host**: `priorities` ranks the
  three layer kinds, not individual injection languages ("python virt above
  host, sql virt below host" is not expressible). For position-based requests
  the virt layer is effectively single-language, and document-wide methods
  default to `concatenated`, so the practical need is unproven. If it
  materializes, a host-relative weight on `bridge.<inj>` is the extension
  point — not entries in `priorities`.
- **Semantic tokens**: the progressive-refinement strategy
  (language-server-bridge-request-strategies Strategy 1) is a *temporal*
  merge — native immediately, bridged tokens replacing them later — not an
  ordering. Semantic tokens stay outside this mechanism (native-only today);
  the `merged`-style strategy that brings them in is specified by
  cross-layer-semantic-token-merge, which reuses this decision's `priorities`
  ordering as its sweep-line layer rank.

## Consequences

### Positive

- **Closed enum is typo-safe and self-documenting**: serde rejects unknown
  layer names; the JSON schema enumerates the three values. No reserved-key
  collision with user language names is possible (unlike a
  `"_native"`-style string convention).
- **Stage-1 schema untouched**: this decision adds no fields to
  `AggregationConfig` or `BridgeLanguageConfig`; existing bridge configs
  resolve as before.
- **Defaults preserve behavior**: `["virt", "host", "native"]` with host
  inert (opt-in off) and `preferred` reproduces today's routing exactly.
- **Per-method layer suppression for free**: omitting a layer from
  `priorities` (e.g. dropping native for hover) needs no dedicated disable
  switch — a direct payoff of the allowlist semantics.
- **Wildcard machinery reused**: no new resolver path; method-keyed map works
  like `bridge.<key>.aggregation`.

### Negative

- **Allowlist override footgun**: writing `priorities = ["host"]` silently drops
  virt and native for that method — a user promoting one layer must restate
  the others. Mitigated by schema docs; inherent to allowlist semantics.
- **Two similarly-shaped maps**: `languages.<lang>.layers.aggregation.<method>` and
  `languages.<lang>.bridge.<key>.aggregation.<method>` are both method-keyed
  ordered-allowlist configs sharing the field name `priorities`; users must
  learn which axis each controls. The element types (closed `LayerSource`
  enum vs. open server names) and the nesting position are the guard rail —
  a layer name in a server list is just an unknown server, while a server
  name in a layer list fails deserialization.
- **Jargon collision**: tree-sitter communities use "layer" for injection
  nesting depth (`LanguageTree` layers). Mitigated by the doc comment on
  `LayerSource` ("NOT injection nesting depth") and by the enum values making
  the meaning evident.

### Neutral

- **`strategy` type shared across stages**: one `AggregationStrategy` enum
  serves both. The anticipated stage-2-only `merged` strategy avoids the
  enum-growth-vs-type-split cost entirely: cross-layer-semantic-token-merge
  keeps `merged` off the enum, dispatching semantic-tokens methods to the
  merge path internally rather than exposing a new strategy value.
- **Native participates without a catalog entry**: the native layer has no
  `languageServers` entry and no `enabled` flag; its participation is
  controlled solely by `priorities` membership.

## Alternatives Considered

### A. Reserved layer keys inside a language-level `AggregationConfig`

Reuse `AggregationConfig` at `languages.<lang>.aggregation.<method>` with
reserved strings in `priorities`:

```toml
[languages.markdown.aggregation."textDocument/hover"]
priorities = ["_self", "python", "_native"]
```

**Rejected because**:
- Mixes two namespaces in one field: server names / language names (open,
  user-defined) and layer identifiers (closed). A reader cannot tell what
  kind of name a list entry is without knowing the reserved-key table.
- Typos silently misroute (an unknown string is just "a server that never
  responds") instead of failing deserialization.
- Reserved strings can collide with real language names; the enum cannot.

### B. Role-tagged entries in stage-1 `priorities`

Extend `bridge.<key>.aggregation.priorities` entries with a role tag
(`{ name = "marksman", role = "host" }` or `"host:marksman"`).

**Rejected because**: already rejected as host-document-bridge Alternative A —
cross-target mixing bumps the type surface of stage 1 and conflates dispatch
policy with per-target configuration. This decision reaffirms that rejection
and places the policy one level up instead.

### C. Field named `aggregation` on `LanguageSettings`

Same structure as chosen, but named `aggregation`.

**Rejected because**:
- Two fields named `aggregation` at different nesting levels with different
  value types (server-name priorities vs. layer priorities) invite confusion
  in docs, schema, and resolution code.
- host-document-bridge already rejected a `LanguageSettings.aggregation`
  field (its Alternative C, for host configuration); reusing the name for a
  different purpose would muddy that history.

### D. Naming: `sources` instead of `layers`

`sources` avoids the tree-sitter "layer" jargon collision.

**Rejected because**: "source" is at least as overloaded (source code, source
files), and "layers" matches the mental model this decision is built on
(three result layers). The collision is handled by one doc-comment line.

### E. Preference order with implicit fallback (first draft of this decision)

`priorities` as a pure preference: layers omitted from the list still participate,
appended in default relative order — mirroring the *pre*-wildcard `preferred`
fan-in.

**Rejected because**: it cannot express per-method layer suppression (the
native-for-hover case) and perpetuates exactly the dual list semantics that
aggregation-priorities-wildcard retires. With the unified allowlist rule,
"preference with fallback" remains spellable — list every layer — while
exclusion becomes spellable too.

### F. Default `priorities = ["virt", "native"]` with priorities as the host gate

Exclude host from the default so that listing `"host"` in `priorities` *is* the
host opt-in, making `bridge._self.enabled` redundant.

**Rejected because**: it splits gating across two mechanisms asymmetrically —
virt targets would gate via `bridge.<inj>.enabled` but host via `priorities`
membership — and turning host on per-language would require restating the
full layer order (allowlist override) instead of flipping one flag.
Keeping host in the default order costs nothing: a disabled host layer is an
empty contributor, and `enabled` remains the single opt-in switch defined by
host-document-bridge.
