# Cross-Layer Result Aggregation

**Related Decisions**:
- [host-document-bridge](host-document-bridge.md) — Host bridging schema (`bridge._self`); declared cross-layer combine logic out of scope, which this decision now covers
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method strategies and multi-server merging *within* a bridge target
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model (virt bridges)
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — Wildcard merge machinery reused for the method-keyed map

## Implementation Status

Not implemented. Depends on host-document-bridge (`bridge._self`), which is
also not yet implemented. This decision fixes the configuration schema ahead
of implementation so that host bridging and cross-layer aggregation can land
against a stable config surface.

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

`AggregationConfig.priorities` is a `Vec<String>` of **language server
names** — an open, user-defined namespace scoped to a single bridge target.
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
  otherwise).
- **Consistency with stage-1 semantics**: ordering is a *preference*, not an
  allowlist — unlisted entries still participate, as in the `preferred`
  fan-in.

## Decision Outcome

**Chosen approach**: add a `layers` field to `LanguageSettings` — a
method-keyed map (LSP method name or `_` wildcard) whose values hold a
closed-enum layer order plus an optional strategy. Stage-1 types
(`AggregationConfig`, `BridgeLanguageConfig`) are untouched.

### Schema

```toml
# ---- Built-in defaults (declared in code; not user-facing) ----
[languages._.layers._]
order = ["virt", "host", "native"]   # innermost-first; mirrors "deeper wins"
# strategy: per-method default (concatenated for diagnostics, else preferred)

# ---- User: markdown hover should prefer the host LS, and show both ----
[languages.markdown.layers."textDocument/hover"]
order = ["host", "virt", "native"]
strategy = "concatenated"

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
    /// Layer preference order, highest first. Omitted layers are appended
    /// in default relative order (virt, host, native). NOT an allowlist.
    pub order: Option<Vec<LayerSource>>,
    /// Reuses the stage-1 strategy type; omit for the per-method default.
    pub strategy: Option<AggregationStrategy>,
}

// On LanguageSettings, beside `bridge`:
pub layers: Option<HashMap<String, LayerAggregationConfig>>,
```

### Two-Stage Aggregation

```
Stage 2 (this decision)   layers.order = [virt, host, native]   ← LayerSource enum
                             │      │      └ native handler result
                             │      └ bridge._self.aggregation.priorities  ← LS names
                             └ bridge.<inj>.aggregation.priorities         ← LS names
Stage 1 (exists today)    each layer resolves its own servers into
                          one response per layer before stage 2 runs
```

Each stage owns one namespace. Stage 1 orders **server names** within a
bridge target; stage 2 orders **layers**. The `AggregationStrategy` *type* is
shared (the "how to combine multiple responses" semantics are identical), but
the configured *values* are independent — e.g., diagnostics can be
`concatenated` across layers while `bridge.python.aggregation` keeps
`preferred` among Python servers.

### Semantics

- **`order` is a preference, not an allowlist.** Layers omitted from `order`
  are appended in the built-in relative order. This matches the stage-1
  `preferred` fan-in, where unprioritized servers still participate via
  first-win fallback. Disabling a layer is done where it already lives:
  `bridge._self.enabled` (host), `bridge.<inj>.enabled` (virt). Native has no
  per-method off switch; with `preferred` it only fires when ordered layers
  return nothing.
- **Empty contributors are skipped.** A layer with nothing to say — no native
  implementation for the method, host bridging disabled, no injection at the
  request position — is an empty contributor; `preferred` falls through to
  the next layer.
- **Resolution reuses wildcard machinery.** The method key resolves via
  `resolve_with_wildcard` (method-specific entry inherits unset fields from
  `_`), and the `layers` field participates in the outer language-level
  wildcard/base merge like `bridge` does.
- **Strategy defaults are per-method.** `default_aggregation_strategy_for_method`
  applies unchanged: `concatenated` for diagnostics, `preferred` otherwise.
- **Nested injections stay implicit.** When injections nest
  (markdown → python → sql), the virt layer resolves deepest-first,
  consistent with the semantic-token priority convention (deeper wins). Depth
  order is not user-configurable.

### Out of Scope

- **Per-virt-language ordering relative to host**: `order` ranks the three
  layer kinds, not individual injection languages ("python virt above host,
  sql virt below host" is not expressible). For position-based requests the
  virt layer is effectively single-language, and document-wide methods
  default to `concatenated`, so the practical need is unproven. If it
  materializes, a host-relative weight on `bridge.<inj>` is the extension
  point — not entries in `order`.
- **Semantic tokens**: the progressive-refinement strategy
  (language-server-bridge-request-strategies Strategy 1) is a *temporal*
  merge — native immediately, bridged tokens replacing them later — not an
  ordering. Semantic tokens stay outside this mechanism (native-only today);
  a future `merged`-style strategy may bring them in.
- **Per-method native disable switch**: suppressing the native layer entirely
  for a method is not provided. Revisit if a concrete need appears.

## Consequences

### Positive

- **Closed enum is typo-safe and self-documenting**: serde rejects unknown
  layer names; the JSON schema enumerates the three values. No reserved-key
  collision with user language names is possible (unlike a
  `"_native"`-style string convention).
- **Stage-1 schema untouched**: `AggregationConfig.priorities` remains purely
  a server-name list; existing configs and resolution code are unaffected.
- **Defaults preserve behavior**: `["virt", "host", "native"]` with per-method
  strategy defaults reproduces today's routing for existing configs.
- **Wildcard machinery reused**: no new resolver path; method-keyed map works
  like `bridge.<key>.aggregation`.

### Negative

- **Two similarly-shaped maps**: `languages.<lang>.layers.<method>` and
  `languages.<lang>.bridge.<key>.aggregation.<method>` are both method-keyed
  aggregation-ish maps; users must learn which axis each controls. The
  distinct field names (`order` of enums vs. `priorities` of server names)
  are the guard rail.
- **Jargon collision**: tree-sitter communities use "layer" for injection
  nesting depth (`LanguageTree` layers). Mitigated by the doc comment on
  `LayerSource` ("NOT injection nesting depth") and by the enum values making
  the meaning evident.

### Neutral

- **`strategy` type shared across stages**: one `AggregationStrategy` enum
  serves both. A future stage-2-only strategy (e.g., temporal `merged`) would
  force either enum growth visible to stage 1 or a type split — acceptable
  deferred cost.
- **Native participates without a catalog entry**: the native layer has no
  `languageServers` entry and no `enabled` flag; it is simply last in the
  default order.

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
  value types (server-name `priorities` vs. layer `order`) invite confusion
  in docs, schema, and resolution code.
- host-document-bridge already rejected a `LanguageSettings.aggregation`
  field (its Alternative C, for host configuration); reusing the name for a
  different purpose would muddy that history.

### D. Naming: `sources` instead of `layers`

`sources` avoids the tree-sitter "layer" jargon collision.

**Rejected because**: "source" is at least as overloaded (source code, source
files), and "layers" matches the mental model this decision is built on
(three result layers). The collision is handled by one doc-comment line.
