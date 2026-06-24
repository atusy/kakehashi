# Host Document Bridge

**Related Decisions**:
- [language-server-bridge](language-server-bridge.md) — Bridge concept introduction
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model (virt bridges)
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method bridge strategies
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — Wildcard config inheritance (foundation for `_self` resolution)
- [push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md) — Diagnostic forwarding
- [cross-layer-aggregation](cross-layer-aggregation.md) — Cross-layer (native/host/virt) result aggregation; covers what this decision scopes out

## Implementation Status

Partially implemented:

- **Schema & gate**: the `_self` reserved key is live. Opt-in is the explicit
  `bridge._self.enabled = true` (`LanguageSettings::is_host_bridging_enabled`);
  the built-in `_self.enabled = false` default is implemented as an
  explicit-only rule — the `_` wildcard's `enabled = true` deliberately does
  not leak into `_self` (equivalent to the Wildcard Merge Safety reasoning
  below, without materializing built-in default entries). Aggregation fields
  DO wildcard-merge (`resolve_host_aggregation`).
- **Dispatch**: implemented for every bridged request method. Because the
  host path needs no URI synthesis or coordinate translation, all methods
  share one generic forwarder
  (`LanguageServerPool::send_host_raw_request`): the upstream request's
  params are forwarded **verbatim** as raw JSON (they already reference the
  real URI and real coordinates) and the result comes back untranslated —
  no per-method request builders or response transformers. Handlers run the
  layer walk (`Kakehashi::walk_layers`, cross-layer-aggregation,
  `preferred` semantics): layers are tried lazily in `priorities` — by default
  virt first, host as fallback. Covered: definition, hover, declaration,
  typeDefinition, implementation, references, completion, signatureHelp,
  documentHighlight, rename, prepareRename, linkedEditingRange, moniker,
  inlayHint, documentSymbol, documentLink, foldingRange, codeLens,
  formatting, and rangeFormatting (which shares the formatting layer key).
  Diagnostics are covered with real cross-layer `concatenated` (the
  cross-layer-aggregation diagnostics phase): pull and synthetic push both
  merge host-server pulls (real URI) with the virt regions' results per the
  layer strategy. Not covered: semantic tokens (native-only) and the experimental
  documentColor/colorPresentation pair. `completionItem/resolve` routes by
  the envelope the virt fan-out stamps into `CompletionItem.data`; host
  completion items carry no envelope and resolve falls back gracefully
  (item returned unresolved). Formatting additionally supports the cross-layer
  `concatenated` pipeline: virt region edits apply first, the host
  formatter formats the intermediate text, and the chain collapses into one
  whole-document replacement edit. During that pipeline the host server's
  document state is briefly speculative (it sees the virt-applied text);
  the lazy fingerprint sync restores the editor text on the next request.
- **Document sync deviation**: instead of forwarding `didChange` params
  verbatim, sync is *lazy*: `didOpen` with the full host text fires on the
  first request per `(uri, server)`, and a **full-text** `didChange` fires
  when the host text's fingerprint changed since the last request. This
  matches the virt path's full-content `didChange` forwarding and avoids
  hooking the concurrent upstream `didChange` stream; verbatim forwarding
  remains the target if eager sync proves necessary.
- **Save notifications fan out to both layers; `willSaveWaitUntil` stays
  host-only** (#357). The save *notifications* concern the host save but are
  forwarded to BOTH bridge layers:
  - **`willSave`** goes to host servers that already have the host document
    open *and* to every open virtual document (URI rewritten to the virtual
    one, reason verbatim);
  - **`didSave`** goes to every open virtual document (URI rewritten); it is
    not forwarded to host servers today (the host `didSave` handler drives the
    synthetic-diagnostic pull instead).

  Each recipient is **gated per-server** on the relevant capability —
  `willSave` on `textDocumentSync.willSave`, `didSave` on `textDocumentSync.save`
  — which is also the safety valve: a virt server only hears about a fragment
  "save" if it opted into save hooks; one that didn't never sees it. The
  `didSave` gate additionally **excludes servers that demand
  `save.includeText = true`**: kakehashi advertises `includeText = false`
  upstream and so never receives the editor's saved bytes, so rather than send a
  contract-violating textless didSave it declines (the server still has current
  content from didChange). That gate reads **static** capabilities only — a
  *dynamic* didSave registration is not honored for forwarding, since the
  method-name-only dynamic registry cannot carry `includeText` and could
  otherwise smuggle an `includeText = true` server past the filter. Both are
  fire-and-forget (no lazy spawn). `willSave` is advertised whenever a runnable
  bridge server (host or virt) is configured; `didSave` is always advertised to
  the editor (`save.includeText = false`).

  **`willSaveWaitUntil` (the request) remains host-only** and bypasses the
  layer walk: it forwards verbatim and returns the host servers' `TextEdit[]`
  via the `preferred` host aggregation, bounded by a 5s budget so a slow server
  cannot hang the editor's save. It is advertised only when some language
  enables host bridging. Fanning the *request* out to virt would need
  virtual→host edit translation and cross-region aggregation that overlap the
  concatenated formatting pipeline (format-on-save), so virt `willSaveWaitUntil`
  stays deferred.
- **Diagnostics**: a `_self` host server's pushed `publishDiagnostics` for the
  real host URI are propagated to the editor via the per-host diagnostic cache
  (push-propagation-diagnostic-forwarding, #421) — accepted when the URI names an
  open host-bridged document. The host document is opened eagerly on each `_self`
  host server at host `didOpen` (#429), so a push-only host server pushes on open
  rather than only after the first request; and it is re-synced on edit at the
  debounced diagnostic cadence (#431), so a push-only host server (skipped by the
  capability-gated pull) re-analyzes current text rather than stale text after a
  change.

## Context

Today kakehashi bridges LSP requests only to **injection regions** (virtual documents): a Python LS handles the Python code blocks inside a Markdown file, an SQL LS handles SQL inside a Rust string, etc. The host document itself — the Markdown, the Rust file as a whole — is parsed by kakehashi but receives no support from a *host* language server. Operations that require whole-document semantics (e.g., marksman on `.md`, or a Markdown-aware formatter) cannot be wired through kakehashi.

Extending bridging to host documents unlocks:

1. **Whole-document LSP for prose/structured formats**: marksman/markdown-ls on `.md`, yaml-language-server on `.yaml`, etc., while injections continue to be served by virt bridges.
2. **Same-language host + virt**: pyright serving both `.py` files (host) and Python injections inside `.md` (virt) through one coherent config.

Design challenges:

1. The existing `bridge` map (`HashMap<String, BridgeLanguageConfig>`) is keyed by **injection language**. There is no slot for "the host language itself."
2. The LS catalog (`languageServers.<name>`) currently has no notion of "host-capable" vs "virt-capable." Adding flags risks surface bloat; omitting them risks ambiguity.
3. Backward compatibility: existing configs must keep current behavior, since host bridging is a new feature.

## Decision Drivers

- **Minimal schema disruption**: no new types in `BridgeServerConfig`, `BridgeLanguageConfig`, or `AggregationConfig`.
- **Reuse wildcard machinery**: wildcard-config-inheritance's `resolve_with_wildcard` should apply uniformly across host and virt entries.
- **Capability vs. policy separation**: `languageServers.*` declares *what* an LS can do; `languages.*.bridge.*` decides *whether and how* it is used.
- **Opt-in for new behavior**: host bridging defaults *off* so existing configs are unchanged.
- **Symmetric mental model**: host and virt are both "bridges" — only the LS-matching rule differs.

## Decision Outcome

**Chosen approach**: Reserve `_self` as a special key in the `bridge` map. It represents the host language acting as its own bridge target. `BridgeLanguageConfig` is reused unchanged. Defaults are declared explicitly at `languages._.bridge._self` (enabled = false) and `languages._.bridge._` (enabled = true), so the existing wildcard merge naturally yields the right answers without special-case resolver logic.

### Schema

```toml
# ---- Built-in defaults (declared in code; not user-facing) ----
[languages._.bridge._self]
enabled = false              # Host bridging is opt-in.

[languages._.bridge._]
enabled = true               # Virt bridging stays default-on (wildcard-config-inheritance).

# ---- User opts markdown into host bridging ----
[languages.markdown.bridge._self]
enabled = true

[languages.markdown.bridge._self.aggregation."textDocument/hover"]
priorities = ["marksman"]

# ---- Virt bridging is configured exactly as before ----
[languages.markdown.bridge.python]
enabled = true

[languages.markdown.bridge.python.aggregation._]
priorities = ["pyright"]

# ---- LS catalog: capability declarations only ----
[languageServers.marksman]
cmd = ["marksman", "server"]
languages = ["markdown"]

[languageServers.pyright]
cmd = ["pyright-langserver", "--stdio"]
languages = ["python"]
```

### Reserved Keys in the `bridge` Map

| Key | Meaning | Field-level wildcard fallback |
|---|---|---|
| `_` | "any injection target" (virt default) | n/a — `_` is itself the wildcard |
| `_self` | "host language itself" (host target) | falls back into `_` during normal merge, but explicit `languages._.bridge._self` defaults keep `enabled` / role-relevant fields key-specific |
| `<language>` | "specific injection target" (virt) | inherits from `_` |

The `_self` ⊕ `_` merge is *not* special-cased in the resolver. It works correctly because, after language-level wildcard merge, `bridge._self.enabled` is always `Some(false)` (from the built-in default), and wildcard-config-inheritance's key-specific-wins rule ensures it overrides the virt default of `Some(true)` when both are present in the same `bridge` map. See "Wildcard Merge Safety" below.

### LS Dispatch Rules

Whether an LS is a candidate for a given request depends entirely on the `languages` field on its `BridgeServerConfig`:

- **Virt path** (`bridge.<inj>` route): select LSes where `languages` contains `<inj>` (the injection language).
- **Host path** (`bridge._self` route): select LSes where `languages` contains `<host>` (the host language of the document).

The same LS naturally serves both roles when applicable. `pyright` with `languages = ["python"]` is a host candidate for `.py` files *and* a virt candidate for Python injections inside other host languages — both routes flow through one connection (one entry in the pool keyed by its `ConnectionKey`, i.e. `(server_name, resolved root)`).

No new fields on `BridgeServerConfig`. An LS that should not act as host for a given language is excluded by leaving `bridge._self.enabled = false` for that language, or by not listing the language in its `languages` field.

### Wildcard Merge Safety

Concern: under wildcard-config-inheritance, `resolve_with_wildcard(map, "_self", merge)` merges the `_` wildcard into the `_self` entry. If `_.enabled = true` and `_self.enabled` were absent, the wildcard would silently turn host bridging on.

Resolution: built-in defaults at `languages._.bridge._self.enabled = false` and `languages._.bridge._.enabled = true` mean that after the *outer* wildcard merge (language layer), every `bridge` map sees `_self` populated with `Some(false)`. During the *inner* wildcard merge (`_self ⊕ _`), key-specific fields win — `_self.enabled = Some(false)` beats `_.enabled = Some(true)`. No special case is needed.

Trace for an unconfigured language `lua`:

```
1. Outer merge:  languages.lua ⊕ languages._
                 → lua.bridge = {_self: {enabled: false}, _: {enabled: true}}
2. Inner merge for bridge._self:
                 _self.enabled = Some(false)  ⊕  _.enabled = Some(true)
                 → Some(false) wins  ✓ host bridging stays off
3. Inner merge for bridge._:
                 _.enabled = Some(true)
                 → Some(true)         ✓ virt bridging stays on
```

Trace when user opts markdown in:

```
User: languages.markdown.bridge._self.enabled = true
1. Outer merge: markdown.bridge = {_self: {enabled: true}, _: {enabled: true (default)}}
2. Inner merge for bridge._self:
                _self.enabled = Some(true) ⊕ _.enabled = Some(true)
                → Some(true)               ✓ host bridging on
```

The same reasoning extends to any future `_self`-meaningful field: as long as the field carries an explicit default at `languages._.bridge._self`, the wildcard merge is safe without resolver changes.

### URI and Coordinate Handling

Host bridges use the **real URI** as sent by the client. This is the key distinction from virt bridges (language-server-bridge-virtual-document-model):

| Aspect | Virt bridge | Host bridge |
|---|---|---|
| URI in `textDocument/didOpen` | `vhost://...` synthesized | client URI verbatim |
| Document text | sub-extracted from host | client text verbatim |
| `didChange` params | injection-range deltas synthesized | forwarded verbatim |
| Response position/range fixup | required (virt → host coordinates) | identity |
| `publishDiagnostics` URI | translated to host URI | passed through unchanged |

Practical consequences:

- `compute_included_ranges` / `sub_select_included_ranges` / virtual URI generation remain virt-only.
- The pool's `(uri, connection key)` host-document sync state handles host with no modification — host_uri is just another string key.
- `request_id.rs` ID multiplexing is URI-agnostic and serves host without changes.
- The coordinator's response post-processing gains a single role-based branch: `if role == Host { resp } else { fixup(resp) }`.

### Out of Scope

- **Combine logic for host/virt responses at request time**: this decision defines only the schema for declaring host and virt bridges. How responses from both roles are ordered, merged, or routed per method is a separate concern decided at dispatch time, not encoded in the configuration shape — since decided in cross-layer-aggregation (the `layers` field on `LanguageSettings`).
- **Editor connecting to the same LS directly**: if the user's editor talks to marksman in parallel with kakehashi, marksman sees duplicate `didOpen` events. Resolving this is the user's responsibility (route only through kakehashi). Kakehashi does not attempt to detect or mediate.
- **Cross-language priority mixing in `priorities` entries**: the `priorities` field remains a `Vec<String>` of LS names within a single bridge target (`bridge.<inj>` or `bridge._self`). Mixing names from different bridge targets in one list is not supported by this schema.

## Consequences

### Positive

- **Zero new types**: `BridgeServerConfig`, `BridgeLanguageConfig`, `AggregationConfig` all unchanged.
- **Reuses wildcard machinery**: wildcard-config-inheritance's `resolve_with_wildcard` applies uniformly, no host-specific resolver path.
- **Backward compatible**: `_self.enabled = false` default keeps existing configs inert.
- **Granular control**: host bridging is per-host-language; aggregation/priorities are per-method.
- **Symmetric mental model**: virt and host live in the same `bridge` map, with the only operational difference being LS-match key (injection language vs. host language).
- **LS catalog stays capability-pure**: no host/virt role flags on `BridgeServerConfig`; one LS entry naturally serves both roles when its `languages` field matches.
- **Real URI for host simplifies coordinate logic**: existing virt position-mapping code remains virt-only and untouched.

### Negative

- **Two-line opt-in**: users must write both `bridge._self.enabled = true` and per-method `aggregation.<method>.priorities`. Forgetting `enabled = true` produces silent no-response.
- **Reserved key cost**: a hypothetical user language literally named `_self` cannot be addressed via `bridge.<lang>`. Acceptable; `_` is already reserved on the same axis, and `_`-prefix names are conventionally reserved.

### Neutral

- **`_self` joins `_` as the second reserved key** in the `bridge` map. The "`_`-prefixed = reserved" convention is preserved and leaves room for future reservations.
- **Host bridging is opt-in even with a candidate LS configured**: `[languageServers.marksman] languages = ["markdown"]` alone does nothing until `bridge._self.enabled = true` is set for some host language. This is the intended behavior — capability declaration is not consent to use.

## Alternatives Considered

### A. Role-tagged priority entries (mixed host/virt in one list)

Allow a single `priorities` list to mix host and virt entries, distinguished by a `role` field:

```toml
[languages.markdown.bridge.python.aggregation."textDocument/hover"]
priorities = [
    { name = "pyright",  role = "virt" },
    { name = "marksman", role = "host" },
]
# or string sugar: ["virt:pyright", "host:marksman"]
```

**Rejected because**:
- Requires extending `AggregationConfig.priorities` from `Vec<String>` to a tagged structure (or a string-mini-DSL), bumping the type surface.
- Cross-target priority mixing is explicitly out of scope (see *Out of Scope*); `priorities` stays scoped to a single bridge target.
- Host/virt ordering is a dispatch-time concern, not a configuration shape, so encoding role in the schema does not match the responsibility split this decision establishes.

### B. Separate `host_bridge` field parallel to `bridge`

Add a dedicated field on `LanguageSettings` for host configuration, parallel to the existing `bridge` map:

```toml
[languages.markdown.host_bridge.aggregation._]
priorities = ["marksman"]

[languages.markdown.bridge.python.aggregation._]
priorities = ["pyright"]
```

**Rejected because**:
- Introduces a parallel field with semantically identical structure to `bridge.<key>`. Two resolvers, two wildcard rules, two `enabled` flags to keep in sync — for no expressive gain.
- Loses the symmetry that host and virt are both "bridges" — only the LS-matching rule differs.

### C. Top-level `aggregation` field on `LanguageSettings`

Keep `bridge.<inj>` for virt, add a peer `aggregation` field on `LanguageSettings` for host:

```toml
[languages.markdown.aggregation."textDocument/hover"]
priorities = ["marksman"]

[languages.markdown.bridge.python.aggregation._]
priorities = ["pyright"]
```

**Rejected because**:
- Splits bridge configuration into two non-uniform shapes: `bridge.<inj>.aggregation` (nested) vs. `aggregation` (flat). Resolvers diverge.
- `LanguageSettings.aggregation` requires its own `resolve_aggregation` method, duplicating logic on `BridgeLanguageConfig`.
- **Less extensibility**: the value type is `AggregationConfig`, so host inherits only fields defined on `AggregationConfig`. Fields on `BridgeLanguageConfig` itself — most notably `enabled` — have no host counterpart, forcing either an ad-hoc parallel field on `LanguageSettings` (e.g., `host_enabled`) or coverage gaps. Any future `BridgeLanguageConfig` field reopens the same asymmetry.
- Subsumed by treating "host" as just another bridge target (one map, one resolver) per the decision in this decision.

### D. Role flags on `BridgeServerConfig`

Mark each LS as host-capable, virt-capable, or both at the LS catalog level:

```toml
[languageServers.marksman]
cmd = ["marksman", "server"]
languages = ["markdown"]
hostEnabled = true
bridgeEnabled = false
```

**Rejected because**:
- Conflates **capability** (what the LS can speak) with **policy** (whether to use it for a given host language). The catalog should describe the former; usage decisions belong at the use-site.
- The same on/off granularity is already achievable via `bridge._self.enabled` (per-host-language) and `bridge.<inj>.enabled` (per-host/injection pair), without per-LS flags.
- Forces users to flip flags on each LS entry to add a new role, rather than enabling at the language they actually care about.

### E. Naming: `self` vs. `_self`

`self` as the reserved key reads slightly more naturally in TOML.

**Rejected because**:
- Breaks the "`_`-prefix = reserved" convention already established by the `_` wildcard on the same axis.
- Forfeits namespace room for future reserved keys (`_meta`, `_root`, etc.) without inventing a second sigil rule.
