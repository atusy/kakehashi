# Host Document Bridge

**Related Decisions**:
- [language-server-bridge](language-server-bridge.md) — Bridge concept introduction
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model (virt bridges)
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method bridge strategies
- [wildcard-config-inheritance](wildcard-config-inheritance.md) — Wildcard config inheritance (foundation for `_self` resolution)
- [push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md) — Diagnostic forwarding
- [bridge-routing-protocol](bridge-routing-protocol.md) — Host-document routing queries read the `_self` aggregation entry and are gated on `_self.enabled`
- [cross-layer-aggregation](cross-layer-aggregation.md) — Cross-layer (native/host/virt) result aggregation; covers what this decision scopes out

## Implementation Status

Partially implemented:

- **Schema & gate**: the `_self` reserved key is live. Opt-in is the explicit
  `bridge._self.enabled = true` (`LanguageSettings::is_host_bridging_enabled`);
  `enabled` is read with a direct `get(_self)` defaulting to `false`, so the
  `_` wildcard's `enabled = true` (the virt default) cannot leak into `_self`
  and the shipped defaults declare no `_self` entry at all. Aggregation fields
  DO wildcard-merge (`resolve_host_aggregation`). See Wildcard Merge Safety
  below for why the apparent inconsistency must stay.
- **Dispatch**: implemented for every bridged request method. Because the
  host path needs no URI synthesis or coordinate translation, all methods
  share one generic forwarder
  (`LanguageServerPool::send_host_raw_request`): the upstream request's
  params are forwarded **verbatim** as raw JSON (they already reference the
  real URI and real coordinates) and the result comes back untranslated —
  no per-method request builders or response transformers. Handlers run the
  layer walk (`Kakehashi::walk_layers`, cross-layer-aggregation,
  `preferred` semantics): layers are tried lazily in `priorities` — by default
  virt first, host as fallback. Seven method families consume per-server identity in
  the host arm: codeAction for the `"{title} — {server}"` suffix, completion
  and inlayHint for their resolve-routing envelopes, codeLens and
  documentLink for the winning server's resolve capability and envelope, and
  call hierarchy and type hierarchy for follow-up routing.
  CodeAction, completion, and inlayHint build custom host arms; codeLens and
  documentLink use the shared whole-document winner hook. Covered: definition, hover, declaration,
  typeDefinition, implementation, references, completion, signatureHelp,
  documentHighlight, rename, prepareRename, linkedEditingRange, moniker,
  inlayHint, inlineValue, documentSymbol, documentLink, documentColor, colorPresentation,
  foldingRange, codeLens,
  prepareCallHierarchy, incomingCalls, outgoingCalls, prepareTypeHierarchy,
  supertypes, subtypes,
  formatting, rangeFormatting (which shares the formatting layer key), and
  semanticTokens/range.
  Diagnostics are covered with real cross-layer `concatenated` (the
  cross-layer-aggregation diagnostics phase): pull and synthetic push both
  merge host-server pulls (real URI) with the virt regions' results per the
  layer strategy. Not covered: semanticTokens/full and full/delta (native-only).
  `completionItem/resolve` routes by
  the envelope stamped into `CompletionItem.data`; the host layer stamps one
  too (marked `host_layer`, so the resolve forwards VERBATIM — no coordinate
  translation and no injection-region edit guard). Both layers mint under
  one rule, shared by completion, codeLens, documentLink, and inlayHint: for a server
  that advertises the matching resolve method, plus the reserved-key
  exception below. Without that capability the items stay bare — an
  envelope would be pure wire weight on every item, and its resolve would
  only fail soft — so a client resolve passes the bare item through
  unchanged. The resolve side re-checks the capability on the live origin
  regardless and, finding it absent, returns the item unresolved with its
  envelope restored — warning that the capability was withdrawn under the
  item (a respawn or dynamic unregister), unless the payload itself nests
  the reserved key, which the resolve side reads as the steady state of a
  non-resolving origin's collision wrap and logs quietly (a resolving origin
  with such a payload loses the warning; accepted). That collision
  exception wraps `data.kakehashi` as opaque `inner` data so a downstream
  payload cannot impersonate bridge routing metadata. (codeAction needs no
  such exception: an action that leaves without an envelope leaves with no
  `data` at all.) `codeLens/resolve` forwards the original payload and
  coordinates verbatim on the host layer. On both layers the codeLens
  envelope retains the host-document incarnation plus the exact producing
  `ConnectionKey` and its generation, so an old lens is returned unresolved
  after either a document reopen, a rooted/shared routing-key change, or a
  downstream process replacement. Resolve looks the producer up by that key
  and never spawns or selects a replacement for process-owned lens data.
  Ordinary downstream failures remain fail-soft,
  while an upstream client cancellation returns `RequestCancelled` and
  cancels the in-flight downstream resolve.
  `documentLink/resolve` uses the same origin, incarnation, connection-key,
  generation, reserved-key collision, and cancellation rules. It preserves the
  already-translated range while allowing the producer to materialize `target`,
  `tooltip`, and updated opaque `data`; a moved or invalidated virtual region
  returns the original unresolved link. Both layers bind opaque link data to
  the exact producing process: resolve never respawns or selects a replacement
  connection after a restart, configuration reroute, or pool-key change.
  `inlayHint/resolve` follows the same exact-producer and cancellation contract.
  The host layer forwards coordinates verbatim; the virtual layer reverses the
  hint position, existing accept edits, and same-document label locations for
  the request, then translates lazily materialized edits and locations back to
  the host. Resolve merges only the protocol's lazy fields (`tooltip`,
  `textEdits`, and label-part `tooltip` / `location` / `command`) into the
  original hint, preserving its position, label values, kind, padding, and
  opaque data. Every host-content revision invalidates a produced hint even
  when the edit preserves the region's shape; this revision check runs both
  before downstream dispatch and after its response. Runtime-adjusted region
  geometry (`#offset!` / `#trim!`) also participates in the pre-dispatch
  freshness check, and
  non-contiguous combined injections fail soft before dispatch because a lazy
  edit could otherwise cross a masked host-only gap. Safe resolves apply the
  same all-or-nothing edit guard as initial hint retrieval.
  `callHierarchy/incomingCalls` and `callHierarchy/outgoingCalls` follow the
  same exact-producer contract.
  Preparation stamps each item with host content/incarnation, connection key
  and generation, region geometry, and whether its URI/ranges were projected
  from a virtual document. Expansion reverses only projected items, strips
  progress and partial-result tokens that the bridge cannot transform, and
  re-envelopes returned callers/callees for recursive expansion. Outgoing
  `fromRanges` are caller-relative, so results translate them with the request
  item's region offset only when that caller was projected from a virtual
  document, regardless of whether the callee is virtual or real. Expansion rejects stale
  content, reopen incarnations, moved/non-contiguous regions, and replaced
  producers before dispatch, then rechecks content and producer identity after
  the response; cancellation targets the exact downstream request. With both
  expansion directions implemented, kakehashi advertises upstream
  `callHierarchyProvider`.
  `textDocument/prepareTypeHierarchy`, `typeHierarchy/supertypes`, and
  `typeHierarchy/subtypes` follow the same preparation/exact-producer contract:
  host items are enveloped without coordinate changes, while same-region
  virtual items are projected to host coordinates and cross-region virtual
  items are dropped. The envelope preserves the exact producer metadata needed
  by expansion. Supertypes and subtypes restore only projected items to the
  producing virtual URI, reject stale documents/regions/processes, and re-envelope
  results for recursive traversal. With both directions implemented, kakehashi
  advertises upstream `typeHierarchyProvider`.
  Exact virtual-URI provenance survives `didClose` because downstream indexes
  may still return closed documents. To keep this generation history bounded,
  the pool retires and recreates a producer before admitting another request
  once 65,536 canonical, scratch, or reserved aliases have been admitted.
  A URI becomes exact provenance synchronously when `didOpen` enters the FIFO,
  before its opened-state promotion can await. Retiring the process clears its
  index and removes the matching generation from the registry; request-scoped
  `Arc` leases retain that generation's provenance only until their in-flight
  responses finish, so replacement cannot change URI classification mid-response.
  Formatting additionally supports the cross-layer
  `concatenated` pipeline: virt region edits apply first, the host
  formatter formats the intermediate text, and the chain collapses into one
  whole-document replacement edit. During that pipeline the host server's
  document state is briefly speculative (it sees the virt-applied text);
  the lazy fingerprint sync restores the editor text on the next request.
- **Document sync deviation**: instead of forwarding `didChange` params
  verbatim, sync sends **full-text** `didChange` with downstream-generated
  versions whenever the host text's fingerprint changed since the last sync
  (which may have been an eager one rather than a request). `didOpen` fires eagerly at upstream `didOpen` on every `_self`
  server (#429, below) and lazily on first request for any server that
  missed it. This
  matches the virt path's full-content `didChange` forwarding and avoids
  hooking the concurrent upstream `didChange` stream; verbatim forwarding
  remains the target if eager sync proves necessary.
- **Save notifications fan out to both layers; `willSaveWaitUntil` stays
  host-only** (#357). The save *notifications* concern the host save but are
  forwarded to BOTH bridge layers:
  - **`willSave`** goes to host servers that already have the host document
    open *and* to every open virtual document (URI rewritten to the virtual
    one, reason verbatim);
  - **`didSave`** goes to host servers that already have the real document open
    and to every open virtual document (URI rewritten for the latter). Before
    an eligible host server receives the notification, kakehashi queues any
    pending full-text `didChange` and `didSave` under the same
    connection/document lock, so save hooks observe the editor's latest text.
    Virtual forwarding binds the parse snapshot and projected content to the
    exact incarnation/content version observed at save time, revalidates that
    version under the edit lock, and queues a required injection `didChange`
    before `didSave` under the virtual document's transition lock. If the 200ms
    settle expires, the document advances, or the `didChange` enqueue fails, it
    drops the virtual `didSave` instead of running a save hook on stale or later
    unsaved fragment text. At ingress, `didSave` is a per-document writer fence,
    so a later wire-order `didChange` cannot overtake save-time version capture.
    Synthetic diagnostic collection registers an abortable background waiter
    for that exact saved incarnation/version and snapshots only after its tree
    is ready; parse or parser-install latency therefore neither blocks the
    writer nor silently loses the save trigger. Snapshot preparation repeats
    that lineage check atomically with the snapshot read. A later `didChange`
    aborts the saved pull before mutating the document, and the completed pull
    revalidates and publishes under the same edit lock, so neither the
    wait-to-snapshot window nor an in-flight downstream request can commit
    diagnostics for an obsolete saved version. Background diagnostic ownership
    is ordered by `(incarnation, content version, settings generation, trigger)`, with
    `didSave > didChange > didOpen` at an equal version and settings generation;
    a late parse/debounce registration therefore cannot cancel the
    already-registered save trigger, while a newer configuration generation
    can legitimately supersede it.
    Tasks remain behind a start gate until that ownership registration wins;
    shutdown permanently closes registration before aborting current tasks.

  Each recipient is **gated per-server** on the relevant capability —
  `willSave` on `textDocumentSync.willSave`, `didSave` on `textDocumentSync.save`
  — which is also the safety valve: a virt server only hears about a fragment
  "save" if it opted into save hooks; one that didn't never sees it. The
  `didSave` reads the server's **static** `save.includeText` preference. When it
  is true, kakehashi attaches the tracked host text or the exact projected
  virtual text bound to that save; otherwise it sends a textless notification.
  A *dynamic-only* didSave registration is not honored for forwarding, since
  the method-name-only dynamic registry cannot retain `includeText`. Both are
  fire-and-forget (no lazy spawn). Host recipients receive the real URI
  verbatim; virtual recipients receive their projected URI. `willSave` is advertised whenever a runnable
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
- **Reuse wildcard machinery**: wildcard-config-inheritance's `resolve_with_wildcard` should apply to host entries wherever it can, so host bridging needs as little bespoke resolver logic as possible.
- **Capability vs. policy separation**: `languageServers.*` declares *what* an LS can do; `languages.*.bridge.*` decides *whether and how* it is used.
- **Opt-in for new behavior**: host bridging defaults *off* so existing configs are unchanged.
- **Symmetric mental model**: host and virt are both "bridges", differing in the LS-matching key and in how `enabled` resolves.

## Decision Outcome

**Chosen approach**: Reserve `_self` as a special key in the `bridge` map. It represents the host language acting as its own bridge target. `BridgeLanguageConfig` is reused unchanged.

Aggregation fields under `_self` resolve through the ordinary wildcard merge (`resolve_host_aggregation`). The `enabled` field does **not** — it is read with a direct `get(_self)` and defaults to `false`, so absence is the off state and the shipped defaults declare no `_self` key at all.

### Schema

```toml
# ---- Built-in defaults (declared in code; not user-facing) ----
# There is deliberately NO [languages._.bridge._self] entry: host bridging is
# opt-in, and `is_host_bridging_enabled` reads `_self.enabled` directly, so
# absence already means off.

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
| `_self` | "host language itself" (host target) | aggregation fields fall back into `_` during normal merge; `enabled` does not — it is read directly, so absence means off |
| `<language>` | "specific injection target" (virt) | inherits from `_` |

The opt-in is enforced by `is_host_bridging_enabled`, which reads `bridge._self.enabled` with a **direct** `get(HOST_BRIDGE_KEY)` and no wildcard fallback, defaulting to `false`. The shipped defaults deliberately contain **no** `_self` key at all, so absence is the off state.

This is worth stating precisely because the obvious refactor is wrong: routing this lookup through `resolve_with_wildcard`, like the aggregation fields legitimately are (`resolve_host_aggregation`), would let the shipped `bridge._.enabled = true` virt default flow into `_self` and turn host bridging on for every language — and, since the host axis selects servers through the same `languages` list, for every `languages = ["*"]` server too (any-language-server-wildcard). `default_settings_has_wildcard_language_with_bridge_defaults` asserts the absence so a defaults change fails loudly.

### LS Dispatch Rules

Whether an LS is a candidate for a given request depends entirely on the `languages` field on its `BridgeServerConfig`:

- **Virt path** (`bridge.<inj>` route): select LSes whose `languages` matches `<inj>` (the injection language).
- **Host path** (`bridge._self` route): select LSes whose `languages` matches `<host>` (the host language of the document).

Matching is list membership, except that the element `"*"` matches every
language (any-language-server-wildcard) — so a `"*"` LS is a candidate on both
paths for every language.

The same LS naturally serves both roles when applicable. `pyright` with `languages = ["python"]` is a host candidate for `.py` files *and* a virt candidate for Python injections inside other host languages — both routes flow through one connection (one entry in the pool keyed by its `ConnectionKey`, i.e. `(server_name, resolved root)`).

No new fields on `BridgeServerConfig`. An LS that should not act as host for a given language is excluded by leaving `bridge._self.enabled = false` for that language, or by not listing the language in its `languages` field — the latter being unavailable for a `"*"` LS (any-language-server-wildcard), which only the `_self` gate can exclude.

### Wildcard Merge Safety

Concern: under wildcard-config-inheritance, `resolve_with_wildcard(map, "_self", merge)` merges the `_` wildcard into the `_self` entry. The shipped `_.enabled = true` is the *virt* default; if it reached `_self`, the wildcard would silently turn host bridging on for every language.

Resolution: `enabled` is exempted from that merge. `is_host_bridging_enabled` reads `bridge._self.enabled` with a **direct** `get(HOST_BRIDGE_KEY)` and `unwrap_or(false)`; there is no `_self` entry in the built-in defaults for `_` to merge into, and none is wanted.

```text
Unconfigured language `lua`:
    lua.bridge = {_: {enabled: true}}          # after the language-layer merge
    is_host_bridging_enabled → get("_self") = None → false   ✓ host off
    is_language_bridgeable("python") → resolves via `_` → true  ✓ virt on

User opts markdown in — languages.markdown.bridge._self.enabled = true:
    markdown.bridge = {_self: {enabled: true}, _: {enabled: true}}
    is_host_bridging_enabled → get("_self").enabled = Some(true) → true  ✓ host on
```

**Do not "simplify" this to `resolve_with_wildcard`.** It looks like an inconsistency worth removing, and removing it enables host bridging globally — including for every `languages = ["*"]` server (any-language-server-wildcard), since the host axis selects servers through the same `languages` list. Two tests hold the line: `host_bridging_not_enabled_by_bridge_wildcard` (the `_` wildcard must not leak in) and `default_settings_has_wildcard_language_with_bridge_defaults` (the defaults must not grow a `_self` key).

Any *future* `_self`-meaningful field has the same choice to make: inherit from `_` like the aggregation fields, or be read directly like `enabled`. Fields whose virt default would be wrong for the host role belong in the second group.

### URI and Coordinate Handling

Host bridges use the **real URI** as sent by the client. This is the key distinction from virt bridges (language-server-bridge-virtual-document-model):

| Aspect | Virt bridge | Host bridge |
|---|---|---|
| URI in `textDocument/didOpen` | `vhost://...` synthesized | client URI verbatim |
| Document text | sub-extracted from host | client text verbatim |
| `didChange` params | injection-range deltas synthesized | synthetic full-text, downstream-generated version |
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
- **Reuses wildcard machinery**: wildcard-config-inheritance's `resolve_with_wildcard` covers the aggregation fields; only `enabled` needs a direct read.
- **Backward compatible**: an absent `_self` reads as disabled, so existing configs are inert.
- **Granular control**: host bridging is per-host-language; aggregation/priorities are per-method.
- **Symmetric mental model**: virt and host live in the same `bridge` map, differing operationally in the LS-match key (injection language vs. host language) and in `enabled` resolution (direct vs. wildcard-merged).
- **LS catalog stays capability-pure**: no host/virt role flags on `BridgeServerConfig`; one LS entry naturally serves both roles when its `languages` field matches.
- **Real URI for host simplifies coordinate logic**: existing virt position-mapping code remains virt-only and untouched.

### Negative

- **Silent no-response when the flag is missed**: `bridge._self.enabled = true` is the whole opt-in — absent `priorities` resolve to `["*"]` (aggregation-priorities-wildcard), so no per-method config is required once a matching server exists. But forgetting `enabled = true` produces no response and no error.
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
- Loses the symmetry that host and virt are both "bridges" living in one map.

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
- Subsumed by treating "host" as just another bridge target in the same `bridge` map per the decision above.

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
