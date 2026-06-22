# Push-Propagation Diagnostic Forwarding

**Related Decisions**:
- [cross-layer-aggregation](cross-layer-aggregation.md) — How virt/host layers combine into one published result
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method bridge strategies relevant to diagnostics
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

The prior pull-first approach (pull-first-diagnostic-forwarding) drove diagnostics
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
- **Reverse map**: `resolve_virtual_uri` scans `document_tracker.host_to_virtual`
  (a host→virtual map) to recover `(host_uri, region_id)` from a virtual URI.
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
   The push is cached regardless of how its server is classified: a spontaneous
   push from a pull-driven server is taken too, which is what keeps #380 closed
   even for those servers.
2. Map the notification to a `source`. A virtual URI resolves to `(host_uri,
   region_id)` via the document tracker (discard if it resolves to no live
   region); a push on the real **host** URI that matches an open `_self`
   host-bridge document is the host-layer source (host-document-bridge).
3. Store the notification in a slot keyed by `(host_uri, source, server)` —
   `source` is the `virtual_uri` for a region or the host URI for the host layer —
   retaining a region's diagnostics in **virtual coordinates** (the host layer
   needs no transform, so it stores host coordinates) plus the `content_epoch` the
   push maps to (see Versioning). A pull-driven server's spontaneous push and its
   `pullFallback` pull share this slot; the latest **current-epoch** update for
   that `(source, server)` replaces it (a stale-epoch push is dropped, never
   overwrites — see Versioning), and eligibility/election uses `content_epoch`.
4. Re-merge **all** slots for that `host_uri` and publish the merge to the
   client.

An empty push clears only that slot; the re-merge still carries every sibling
source. Push-driven servers feed this path natively; pull-driven servers feed it
via the `pullFallback` toggle and, opportunistically, via any push they emit (see
Per-server source and fallback).

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

Classification is **live, not an initialize-time snapshot**:
`has_capability("textDocument/diagnostic")` (`connection_handle.rs`) consults the
static initialize result *and* dynamic `client/registerCapability` registrations,
so a server that registers or unregisters pull support mid-session is reclassified
and its fallback channel follows. (Unregister only removes *dynamic* registrations,
so the pull-driven → push-driven transition applies only to dynamic-only
diagnostic providers; a statically-advertised `diagnosticProvider` stays
pull-driven for the session.)

LSP has no capability flag for *push* diagnostics, so push-classification is an
inference from the absence of `diagnosticProvider`, not a declared fact — the
`pullFallback` / `pushFallback` toggles exist precisely to recover the other
channel when a server defies the inference (a pull-driven server that also pushes,
or vice versa).

Native source alone would make the two paths asymmetric — Path B already
back-fills push-driven servers from the cache, but Path A would leave pull-driven
servers with no *proactive* diagnostics. Two config toggles close the gap
symmetrically, **both defaulting to `true`** so every server contributes to both
paths regardless of which single mechanism it supports:

| Method block (`bridge.<lang>.aggregation.<method>`) | Key | Effect when `true` (default) |
| --- | --- | --- |
| `textDocument/publishDiagnostics` (Path A) | `pullFallback` | Pull-driven servers are pulled on host events and merged into the same cache, so they too publish proactively. A pulled result has no `publishDiagnostics.version`, so its slot is stamped with the `content_epoch` that was pulled (the null-version rule below). |
| `textDocument/diagnostic` (Path B) | `pushFallback` | Push-driven servers' cached pushes are folded into the client-pull response. |

Setting a toggle to `false` disables only the **fallback** work for that path —
Path A's fallback pull of pull-driven servers, or Path B's reading of cached
pushes. Spontaneous non-scratch pushes are still cached regardless (step 1), so
disabling `pullFallback` does not re-open the #380 blindness for a pull-driven
server that happens to push on its own; it only stops kakehashi from *pulling* it.

Both are `Option<bool>` raw fields on `AggregationConfig` (`settings.rs`, which is
already `#[serde(rename_all = "camelCase")]`), resolving to the default `true`
when unset, merged field-by-field through the config merge (`merge.rs`), and
serialized as `pullFallback` / `pushFallback`.

Naming note (the prompt's `pullFallbackEnabled` / `pushFallbackEnabled`): the
`Enabled` suffix is dropped to match the existing bare-boolean `enabled` on
`BridgeLanguageConfig` — the field *is* the toggle. The mechanism stays in the
key (rather than a method-disambiguated bare `fallback`) so a single config line
reads unambiguously out of context.

### Cache model

```
host_uri ──► { (source, server) ──► SlotEntry { diagnostics(source-local coords), content_epoch } }
```

`source` is a virtual region's URI or the host URI for the `_self` host layer;
the key also includes the **server**, since several servers can attach to one
source (e.g. two Lua linters on one region) and each pushes independently.
Producing the host publish mirrors the existing staged pull aggregation:

1. **Per-source fan-in** — for each source, combine its servers' slots per the
   method's `strategy`: `concatenated` appends all, `preferred` elects one (see
   `preferred` as a push stream).
2. **Cross-region merge** — concatenate the per-source fan-ins of the virt layer's
   regions, ordered by region start position (so arrival order never leaks),
   lazily transforming each region slot's virtual coordinates to **current** host
   coordinates via the region's current offset (recovered from the stable region
   id).
3. **Cross-layer combine** — combine the virt-layer result with the host-layer
   result per cross-layer-aggregation (host-document-bridge governs host
   participation). The host layer's diagnostics are already host-local, so they
   skip the virtual→host transform.

Re-merging on every push republishes the cumulative host set: when region A
publishes, then the host layer, then region C, each event re-runs the merge and
re-publishes `{A}`, `{A, host}`, `{A, host, C}` — a slot is *replaced* by its
source's latest push, never accumulated.

### Versioning and staleness (the crux)

Storing virtual coordinates rather than pre-baked host coordinates makes the
policy clean:

- **Content epoch vs wire version**: gate and elect on a per-source
  **`content_epoch`** — a monotonic counter (or content fingerprint) that advances
  **only when the source's extracted content changes**, shared across all servers
  on that source. This is *not* the per-connection LSP wire version the bridge
  already tracks (`document_tracker` keys version by `(connection, virtual_uri)`
  and bumps per connection), which is not comparable across servers — a restart or
  late open desyncs it. To map a push back to an epoch, kakehashi records a per
  `(source, connection, wire_version) → content_epoch` entry **when each
  `didOpen`/`didChange` is successfully enqueued to that connection's writer**
  (enqueued, not merely counter-incremented). A non-null `publishDiagnostics.version`
  is an exact lookup, with three outcomes: maps to the **current** epoch → the push
  replaces the slot; maps to an **older** epoch (a late push for content already
  superseded) → **dropped and logged, the slot is not replaced** (so a stale push
  can never overwrite a fresher slot); **not in the map** (protocol mismatch) →
  dropped and logged. Only a current-epoch push replaces a slot. Null versions fall
  back to the best-effort rule below. Eligibility is "slot's epoch ==
  source's current epoch", never a raw cross-server version compare.
- **Lazy re-anchor**: transforming at publish time against the region's *current*
  offset re-positions an unchanged region's diagnostics after an edit *above* it
  with no re-push and no flicker — provided the epoch keys on content, not geometry
  (above), so a position-only edit does not advance it.
- **Version gate**: a slot whose epoch lags the source's current `content_epoch`
  is held (not published) until that server re-publishes at the current epoch,
  bounding the wrong-position window to the re-parse gap and self-healing. The virt
  path currently bumps the wire version and re-sends `didChange` for *every* opened
  region on *every* host `didChange` (`did_change.rs::forward_didchange_to_opened_docs`),
  with no content guard; the host path already fingerprints before syncing
  (`host.rs`). This decision requires the virt sync to gain that content guard so
  the epoch advances only on a real content change.
- **Re-merge on epoch bump**: when a source's `content_epoch` advances, kakehashi
  re-merges and republishes that `host_uri` *immediately*, with the now-stale slots
  held — otherwise nothing would clear the old diagnostics until a current-epoch
  push happens to arrive. The bump itself is the trigger, not just incoming pushes.
- **Re-merge on geometry change**: a host edit that shifts a region's position
  without changing its content does *not* advance `content_epoch`, so lazy
  re-anchor alone would leave the previously-published (now mis-placed) host
  coordinates on screen until an unrelated push. The region-offset update (the
  re-parse that moves the region) must itself trigger a re-merge, republishing only
  if the re-anchored host coordinates actually changed. Lazy re-anchor supplies the
  correct positions; this supplies the missing trigger.
- **Host layer**: the `_self` host source uses the identical model keyed on the
  *host document's* content epoch — push-driven `_self` servers are eagerly opened
  (see Lifecycle) and eagerly re-synced on a content-changing host `didChange`
  (the host path's fingerprint already gates this), so the gate and re-merge rules
  above apply unchanged.

### `preferred` as a push stream

Pull resolves `preferred` synchronously — fan out, take the first non-empty
response in priority order, abort the rest. `fan_in/preferred.rs` shows the exact
rule: a named server holds a strict position and is waited for even if a lower one
answers first, while the `"*"` group is decided by **earliest non-empty arrival**
and then `abort_all`. A push stream never ends, so "abort the rest" becomes "stop
switching away from the elected server". **Named servers keep pull's strict
priority — a higher named server preempts whenever it becomes eligible;
sticky-first applies only *within* the `"*"` first-win group**, where there is no
static order to fall back on. Per virtual document:

- **Effective epoch** `Veff` = the source's current `content_epoch` (see
  Versioning), *not* the highest version seen in arrived pushes — a content change
  advances `Veff` at once, before any push at the new epoch exists. Slots at an
  older epoch are held (treated as absent) until that server re-publishes at
  `Veff`. This is the version gate, and it is what makes a content bump re-open the
  election.
- **Winner walk** over the expanded `priorities`:
  - `Server(name)` — wins if its slot is eligible (present, **non-empty**, at
    `Veff`); otherwise fall through to the next entry.
  - `Rest("*")` group — the **incumbent** keeps the position while it stays
    eligible (sticky); when it loses eligibility, re-elect the earliest-arrived
    eligible member **measured by first arrival at the current `Veff`** (a version
    bump restarts arrival ordering, so a stale incumbent does not retain seniority
    from an old version), and **the new winner becomes the incumbent** (never snap
    back to the original — that ping-pongs the display). The walk reaches the
    group only when every higher named entry is ineligible; a named server
    becoming eligible later preempts the group and the group's incumbency resumes
    if the named server drops out again.
- The first eligible source has no incumbent yet, so the bootstrap winner is
  simply the first server to publish non-empty at `Veff`.
- The region's fan-in result is the winner's latest diagnostics; a push that does
  not change the elected winner's emitted set emits nothing. Equality is compared
  on the slot's **transformed host-coordinate** diagnostics (implementers may
  normalize unstable fields such as `data`), so the suppression survives lazy
  re-anchor only when the host positions are genuinely unchanged.

**Empty falls through (matches pull's "first *non-empty*").** A preferred server
publishing `[]` (clean) does not win — the walk continues, so a unique finding
from a lower-priority server still surfaces. Deliberate consequence: a clean
result from the preferred server does **not** suppress a lower server reporting a
current-version problem. The alternative (empty-wins, where the preferred server's
clean silences everyone below) was rejected — it diverges from pull and hides real
findings; stale-error flicker is prevented by the version gate, not by empty-wins.

**Null `version`.** `publishDiagnostics.version` is optional. A null-version push
is **best-effort**: it is attributed to the `content_epoch` kakehashi most recently
synced to that server. Because the outbound `didChange` increments and queues
asynchronously, a null-version push can race an in-flight content change and
actually describe the *previous* content — so it is gated like any slot (held if
its attributed epoch is now stale), accepting a documented small stale-window risk
rather than treating it as authoritative.

Worked traces (servers `a1,a2,a3` in one `"*"` group; `aN<epoch,nth>`, where
`epoch` is the source `content_epoch` and `nth` the per-server publish count):

- Same version — raw `a2<1,1>, a3<1,1>, a1<1,1>, a3<1,2>, a3<1,3>, a1<1,2>`
  publishes `a2<1,1>` **once**. `a2` is elected on first arrival and stays
  incumbent; every later push (including `a3`'s own revisions) leaves the winner
  unchanged. (Contrast: keying the winner on `nth` would let the chattiest server
  win — loudest ≠ best — so `nth` governs only *within* a server, as slot
  replacement, never *across* servers.)
- Version bump — events `a2<1,1>`, **content-bump→v2**, `a1<2,1>`, `a2<2,1>`.
  Publishes `a2<1,1>`; then the bump advances `Veff` to 2 and re-merges with every
  v1 slot held → publishes **empty** (clears `a2<1,1>`); then `a1<2,1>` is the
  first eligible at v2 → publishes `a1<2,1>`; then `a2<2,1>` arrives but the
  incumbent `a1` is unchanged → no publish.
- Named preemption (`priorities = [a1, "*"]`, all v1) — raw `a2<1,1>, a1<1,1>,
  a2<1,2>` publishes `a2<1,1>` (group bootstrap), then **`a1<1,1>`** (the named
  higher entry becomes eligible and preempts the group), then nothing for
  `a2<1,2>` (a1 still wins). Were `a1` to clear (`[]`), the walk falls back to the
  `"*"` group and its incumbency resumes at `a2`.

### `concatenated` as a push stream

`concatenated` keeps every server, so the fan-in needs no election — but the slot
model and version gate carry over unchanged. Per virtual document:

- Each server's slot holds its **latest** push; a repeat publish at the same
  epoch replaces the previous one (the within-server rule, same as `preferred`).
- The region's result is the **concatenation of all eligible slots in the
  flattened expanded-`priorities` walk** — `Server` entries and the `Rest("*")`
  group each keep their *configured position* (so `["*", "a1"]` puts the group
  first), and the group's members are in server config order excluding names listed
  elsewhere — the order pull already uses, so the output stays stable no
  matter which server pushed last. Arrival order must not leak into the result.
- The version gate applies: only slots at `Veff` are concatenated; a slot lagging
  `Veff` is held until it re-publishes at `Veff`. During a content-epoch bump
  the concat shrinks to the caught-up servers and refills as the slower ones catch
  up — the same self-healing window as elsewhere, not a special case.

`empty` and `absent` coincide here: an empty slot contributes nothing, so the
`preferred` empty-vs-absent question never arises.

Worked traces (servers `a1,a2` in one region, `priorities = [a1, a2]`):

- Same version — `a1<1,1>=[X]`, `a2<1,1>=[Y]`, `a1<1,2>=[X']` → publishes `[X]`,
  then `[X, Y]`, then `[X', Y]`. `a1`'s repeat replaces `X` with `X'`; `a2` is
  untouched; order is fixed by `priorities`, not by who published last.
- Version bump — a **content-bump→v2**, then `a2<2,1>=[Y2]`. The bump advances
  `Veff` to 2 and holds both v1 slots → re-merge publishes **empty** (clears
  `[X', Y]`); then `a2<2,1>` makes the v2 concat `[Y2]` alone (`a1`'s v1 slot still
  held); it refills once `a1` re-publishes at v2.

### Lifecycle

- Host `didClose` → drop the `host_uri` cache entry.
- Region invalidated by an edit (`close_invalidated_docs`) → evict **all** slots
  for that source across servers, then re-merge.
- Downstream crash/restart → drop that server's slots, then re-merge
  (publish-empty-to-clear falls out naturally).
- **Host-layer eager open**: a push-driven `_self` server only pushes once its
  host document is open, but host sync is otherwise lazy — the host doc opens on
  the first client host request (`host.rs`), not on host `didOpen`. So Path A must
  eagerly open the real host document on host `didOpen` for push-driven `_self`
  servers — the host-layer analogue of `eager_open_virtual_documents`. (Pull-only
  `_self` servers do not need this; their `pullFallback` pull opens on demand.)
  Because classification is live, a `_self` server that *unregisters*
  `textDocument/diagnostic` mid-session (pull-driven → push-driven) has already
  missed the host `didOpen`, so the transition into push-driven must itself
  eagerly open any currently-open host docs where that `_self` server is enabled.
  A config change that newly makes a push-driven `_self` server eligible (e.g.
  enabling it, or adding it to `priorities`) must eagerly open in the same way.
- **Re-merge on classification/config change**: a change that alters which slots
  are visible takes effect differently per path. Path A (proactive publish) must
  trigger an immediate host re-merge on any `textDocument/publishDiagnostics`
  contributor/election change — capability reclassification, `pullFallback`, server
  `priorities`/`strategy`/`maxFanOut`, layer priorities/strategy, or bridge/candidate
  enablement — otherwise the new visibility waits for the next diagnostic event.
  Path B (client pull) needs no proactive re-merge: it is recomputed per request,
  so the analogous `textDocument/diagnostic` and `pushFallback` changes simply
  apply on the next client pull.
- **Held-but-silent slot**: a slot held by the version gate whose push-driven
  server never re-publishes at the new content epoch stays hidden until it does.
  A conforming server re-emits after the `didChange`, so it self-heals; a dead
  server is caught by crash/close above; a live-but-silent server is an accepted
  risk (a timeout or clear-on-change could be added later).

Per-URI ordering between incoming pushes and edits is the ordering already relied
on by ls-bridge-message-ordering.

### Position transformation (reuses the existing bridge translation)

Mapping a region slot's virtual coordinates to host coordinates reuses the
bridge's existing virtual→host translation (`translation.rs`): each virtual
UTF-16 `Position` is shifted by the region's `RegionOffset` — a base line plus a
per-virtual-line UTF-16 **column** offset (a single value for ordinary
injections; one per line for blockquote prefixes), applied with saturating
arithmetic for stale-region safety. This is the same path the current pull
diagnostics already transform through (`diagnostic.rs`); the push path adds no new
transform. Lazy re-anchor (Versioning) simply re-applies it against the region's
*current* `RegionOffset` at publish time.

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
   prior pull-first approach): leaks internal virtual URIs and breaks the
   host-as-single-entity abstraction.
8. **`preferred` winner keyed on publish count (`nthPublish`).** Rejected: the
   per-server publish counter measures chattiness, not quality, and is not
   comparable across servers — it makes the loudest server win. `nth` is
   meaningful only *within* a server (later replaces earlier), which sticky-first
   already captures as slot replacement.
9. **`preferred` empty-wins (preferred server's clean suppresses lower servers).**
   Rejected: diverges from pull's "first *non-empty*" for the same `strategy` key,
   and hides a lower server's real current-version findings. Empty falls through
   instead; the version gate handles stale-error flicker.

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

- A per-`(host, source, server)` diagnostic cache with lifecycle handling (host
  close, region invalidation, server crash).
- Version/staleness logic returns (version gate + lazy re-anchor); a brief
  hold/wrong-position window remains right after an edit, self-healing on the next
  push.
- The reader must transform and route pushes, interleaved with edits, relying on
  the per-URI ordering guarantee.
- Re-publish is unthrottled by design (the `didChange` debounce is gone): a no-op
  push is suppressed (winner content unchanged), but a chatty linter emitting
  distinct results in quick succession re-publishes each time. A coalescing window
  could be added later if it proves noisy; it is deliberately out of scope here.
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
implements the prior pull-first synthetic push:

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
the prior pull-first decision.
