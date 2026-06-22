# Push-Propagation Diagnostic Forwarding

**Related Decisions**:
- [cross-layer-aggregation](cross-layer-aggregation.md) — How virt/host layers combine into one published result
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method bridge strategies (this supersedes the diagnostic strategy)
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — Server pool, document tracking, notification forwarding
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — Per-URI ordering the cache relies on
- [host-document-bridge](host-document-bridge.md) — Host-layer (`_self`) participation

## Context

kakehashi bridges injection regions of a host document (e.g. Lua code blocks in
Markdown) to downstream language servers, each region exposed as a *virtual
document*. Diagnostics from those regions must reach the editor mapped onto the
host document.

### The prior approach and its structural limit

The superseded decision (pull-first-diagnostic-forwarding) drove diagnostics
entirely from host-document events: on `didOpen` / `didSave` / `didChange`,
kakehashi *pulled* `textDocument/diagnostic` from every region's downstream
server, aggregated, and published a synthesized result. Downstream-initiated
`publishDiagnostics` notifications were discarded — `forward_notification` in
`src/lsp/bridge/actor/reader.rs` drops them at its `_ => {}` arm (#380).

That model is **structurally blind to diagnostics the downstream emits without a
host edit**: slow linters that finish later, cross-file or build-triggered
diagnostics, watcher-driven updates. The proactive path only re-pulls when the
*host* changes, so anything the downstream surfaces on its own timeline never
reaches the editor until an unrelated host edit happens to re-trigger a pull. It
also pays a re-pull round-trip plus a 500ms `didChange` debounce on every event.

### The clobber constraint

A client replaces *all* diagnostics for a URI each time it receives
`textDocument/publishDiagnostics` for that URI. Every injection region of a host
maps to the **same** host URI, so forwarding one virtual document's push
verbatim would erase every other region's diagnostics — region A's errors vanish
the instant region B publishes. Any push-propagation design must therefore merge
all regions' current diagnostics on every publish, not forward them piecewise.

### What already exists (so the reversal is cheaper than it looks)

pull-first chose pull specifically to avoid the state a push model needs. Most of
that state already exists today for other features:

- **Persistent virtual-doc open**: `eager_open_virtual_documents`
  (`src/lsp/bridge/text_document/did_open.rs`) opens every region's virtual
  document on its downstream server at host `didOpen`, and `didChange` /
  `didClose` are synced (`pool.rs::ensure_document_opened`,
  `did_change.rs`, `did_close.rs`). Downstream servers already *push*
  `publishDiagnostics` for these opened docs — that is exactly why
  `is_scratch_publish_diagnostics` filtering had to be added.
- **Reverse map**: `document_tracker.host_to_virtual` resolves a virtual URI
  back to `(host_uri, region_id)` (`resolve_virtual_uri`).
- **Stable region identity**: `node_tracker` assigns ULIDs that survive edits via
  position-keyed tracking, with `close_invalidated_docs` evicting regions whose
  positions are invalidated — a ready-made eviction hook.

So the `InjectionContextTracker`-equivalent the old ADR feared building is, in
large part, already present.

## Decision

Replace the host-event-driven synthetic pull with **push propagation through a
per-host merge cache**. Two paths share one cache.

### Path A — Proactive publish (new core)

Downstream-initiated `publishDiagnostics` become the source of proactive
diagnostics:

1. The reader routes a non-scratch downstream `publishDiagnostics` into the
   diagnostic aggregator instead of dropping it at `_ => {}`. (Scratch URIs are
   still discarded earlier — concatenated-formatting-pipeline Decision point 7.)
2. Resolve the target virtual URI to `(host_uri, region_id)` via the document
   tracker; discard if it resolves to no live region.
3. Store the notification in a slot keyed by `(host_uri, virtual_uri)`, retaining
   the diagnostics in **virtual coordinates** plus the `version` from the params.
4. Re-merge **all** slots for that `host_uri` and publish the merge to the
   client.

An empty push clears only that slot; the re-merge still carries every sibling
region. Push-driven servers feed this path natively; pull-driven servers feed it
via the `pullFallback` toggle (see Per-server source and fallback).

### Path B — Client-initiated pull (retained)

kakehashi keeps answering `textDocument/diagnostic` from the client:

- Fan out a live `textDocument/diagnostic` to every pull-driven downstream,
  transform and aggregate as today.
- For push-driven downstream, serve their cached push slots from Path A via the
  `pushFallback` toggle.
- Merge and respond. The advertised `diagnosticProvider` capability is unchanged.

Path A writes the cache; Path B reads it for push-driven servers — one cache,
two readers.

### Per-server source and fallback

Each downstream server has exactly one *native* diagnostic source, decided by
capability so the two mechanisms never double-count the same server:

- Advertises `diagnosticProvider` → **pull-driven** (kakehashi pulls it).
- Otherwise → **push-driven** (kakehashi relies on its `publishDiagnostics`).

Native source alone would make the two paths asymmetric — Path B already
back-fills push-driven servers from the cache, but Path A would leave pull-driven
servers with no *proactive* diagnostics. Two config toggles close the gap
symmetrically, **both defaulting to `true`** so every server contributes to both
paths regardless of which single mechanism it supports:

| Method block (`bridge.<lang>.aggregation.<method>`) | Key | Effect when `true` (default) |
| --- | --- | --- |
| `textDocument/publishDiagnostics` (Path A) | `pullFallback` | Pull-driven servers are pulled on host events and merged into the same cache, so they too publish proactively. |
| `textDocument/diagnostic` (Path B) | `pushFallback` | Push-driven servers' cached pushes are folded into the client-pull response. |

Setting a toggle to `false` restricts that path to its native servers only.

Naming note (the prompt's `pullFallbackEnabled` / `pushFallbackEnabled`): the
`Enabled` suffix is dropped to match the existing bare-boolean `enabled` on
`BridgeLanguageConfig` — the field *is* the toggle. The mechanism stays in the
key (rather than a method-disambiguated bare `fallback`) so a single config line
reads unambiguously out of context.

### Cache model

```
host_uri ──► { virtual_uri (source) ──► SlotEntry { diagnostics(virtual coords), virtual_version } }
```

Merge = for each slot, lazily transform its virtual-coordinate diagnostics to
**current** host coordinates using the region's current host offset (recovered
from the stable region id), then combine per cross-layer-aggregation
(`concatenated` over `priorities` by default, or `preferred`). Host-layer
participation follows host-document-bridge.

### Versioning and staleness (the crux)

Storing virtual coordinates rather than pre-baked host coordinates makes the
policy clean:

- **Lazy re-anchor**: transforming at publish time against the region's *current*
  offset means an edit *above* an unchanged region re-positions its diagnostics
  correctly with no re-push and no flicker — this is what the stable region
  identity buys.
- **Version gate**: a slot whose `virtual_version` lags the current virtual
  document version is held (not published) until a matching push arrives. This
  bounds the wrong-position window to the re-parse gap and self-heals as the
  downstream re-emits against the new content.

### Lifecycle

- Host `didClose` → drop the `host_uri` cache entry.
- Region invalidated by an edit (`close_invalidated_docs`) → evict its slot, then
  re-merge.
- Downstream crash/restart → drop that server's slots, then re-merge
  (publish-empty-to-clear falls out naturally).

Per-URI ordering between incoming pushes and edits is the ordering already relied
on by ls-bridge-message-ordering.

### Position transformation (carried forward, unchanged)

```
Virtual (UTF-16) → position_to_byte → + content_start_byte → byte_to_position → Host (UTF-16)
```

## Considered Options

1. **Keep pull-first synthetic push (status quo).** Rejected: structurally blind
   to downstream-initiated async diagnostics (#380); re-pull round-trip plus
   500ms debounce on every host event.
2. **Forward downstream pushes verbatim, no cache.** Rejected: the clobber
   constraint erases sibling regions on every publish.
3. **Cache, but drop all slots for the host on any host edit.** Rejected as
   default: diagnostics vanish and reappear on each keystroke; lazy re-anchor
   avoids the flicker.
4. **Cache, hold stale host coordinates until a re-push (no re-anchor).**
   Rejected as default: shows visibly wrong positions after edits above a region;
   re-anchoring is cheap given stable region identity.
5. **Push-only, drop the client-pull handler.** Rejected: abandons pull-driven
   clients and servers; the client pull path is retained.
6. **Asymmetric paths — push-only Path A, no pull fallback.** Rejected: leaves
   pull-driven servers with no proactive diagnostics while Path B back-fills
   push-driven ones, an unjustified asymmetry. The `pullFallback` / `pushFallback`
   toggles (default `true`) make both paths cover every server.
7. **Client-side aggregation (forward virtual URIs raw).** Rejected (as in the
   superseded decision): leaks internal virtual URIs and breaks the
   host-as-single-entity abstraction.

## Consequences

### Positive

- Captures the downstream's native diagnostic cadence — async linters, cross-file
  and build-triggered diagnostics — that host-event pulling structurally could
  not (#380).
- Removes the per-event re-pull round-trip and the `didChange` debounce from the
  proactive path; diagnostics are event-driven and fresher.
- Reuses existing machinery (persistent virtual-doc open, host↔virtual reverse
  map, stable region identity with an eviction hook), so the reintroduced state
  is materially lighter than pull-first anticipated.

### Negative (accepted cost)

This re-incurs the state pull-first was created to avoid; the decision accepts it
because the #380 benefit now outweighs it:

- A per-`(host, region)` diagnostic cache with lifecycle handling (host close,
  region invalidation, server crash).
- Version/staleness logic returns (version gate + lazy re-anchor); a brief
  hold/wrong-position window remains right after an edit, self-healing on the next
  push.
- The reader must transform and route pushes, interleaved with edits, relying on
  the per-URI ordering guarantee.
- `pullFallback` (default on) keeps a *scoped* host-event pull trigger alive for
  pull-driven servers — the per-event re-pull is removed only for push-driven
  ones, not universally. Setting it `false` drops those servers from the proactive
  path entirely.

### Neutral

- Position transformation and the aggregation strategy are unchanged.
- The client-pull handler is largely unchanged; it now additionally reads the
  push cache for push-driven servers (`pushFallback`).

## Decision–Implementation Gap

Not yet implemented — this decision runs ahead of the code, which still
implements the superseded pull-first synthetic push:

- `src/lsp/lsp_impl/coordinator/diagnostic.rs`
  (`spawn_synthetic_diagnostic_task`, `schedule_debounced_diagnostic`),
  `src/lsp/debounced_diagnostics.rs`, `src/lsp/synthetic_diagnostics.rs`, and the
  pull in `src/lsp/bridge/text_document/diagnostic.rs`.
- `src/lsp/bridge/actor/reader.rs` still discards non-scratch
  `publishDiagnostics` at `forward_notification`'s `_ => {}` arm (#380).

Migration outline: route non-scratch pushes into a new aggregator backed by the
per-host merge cache; reduce the synthetic/debounced *proactive* pull to the
`pullFallback` scope (pull-driven servers only) instead of retiring it outright;
keep the *client* pull handler (augmented with the push cache for push-driven
servers); update the bare-slug code references that currently name
the superseded decision.
