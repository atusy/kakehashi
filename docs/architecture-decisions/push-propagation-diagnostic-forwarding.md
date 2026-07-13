# Push-Propagation Diagnostic Forwarding

**Related Decisions**:
- [cross-layer-aggregation](cross-layer-aggregation.md) — How virt/host layers combine into one published result
- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md) — Per-method bridge strategies relevant to diagnostics
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md) — Server pool, document tracking, notification forwarding
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — Per-URI ordering the cache relies on
- [host-document-bridge](host-document-bridge.md) — Host-layer (`_self`) participation
- [downstream-diagnostic-refresh-handling](downstream-diagnostic-refresh-handling.md) — How a downstream `workspace/diagnostic/refresh` triggers the Path A pull and forwards to the editor

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
3. Store the notification in the nested slot at `host_uri → source → server` —
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

**Wire leading + trailing debounce.** Wire sends are coalesced per host URI
under a workspace-wide method policy:

```toml
[features."textDocument/publishDiagnostics"]
debounceMs = 100
maxWaitMs = 1000
```

The first changed merge after idle passes immediately. Later changes update the
cached latest set and one trailing republish re-merges it after `debounceMs` of
quiet; continuous activity is forced no later than `maxWaitMs` after the previous
send. State is per URI, so a noisy document cannot delay another. Only the wire
waits: change detection, the coverage bump, and the refresh nudge fire at the same
points as before. `didClose` forgets pending state and sends its clearing publish
outside the debounce; shutdown cancels pending tasks. Pull diagnostics are outside
this scheduler.

**Config wire seal.** The cross-layer gate for
`textDocument/publishDiagnostics` doubles as a seal on the editor-facing wire
sends: when `layers.aggregation."textDocument/publishDiagnostics".priorities`
resolves to `[]` for the host's language, `republish` skips the
`publishDiagnostics` send entirely (except `didClose`'s harmless clearing
publish, which fails open by design when the document is gone). This is the
opt-out for pull-first editor setups, where the proactive publish is a
redundant second delivery of the same merged set — the editor either ignores
it or (Neovim ≥ 0.11) renders it as a duplicate diagnostic namespace next to
the pulled one; on a diagnostics-heavy host it dominated the editor pipe
(measured: 51 publishes × ~1 MB in a 44 s typing burst, 68% of all
server→client bytes), head-of-line blocking every other response. At the
`republish` level the seal skips only the wire send: the merge still runs and
records the last-merged set, so change detection keeps driving the
push-origin coverage bump and `workspace/diagnostic/refresh` — the sealed
setup's delivery signal (refresh → client re-pull). The seal is binary (all
layers gated off); a partially gated priorities list still publishes the full
merge — per-layer selection applies to the debounced pull feed, as before.

Seal prerequisites and caveats:

- The delivery signal presumes the client advertises **both**
  `textDocument.diagnostic` (it pulls) and
  `workspace.diagnostics.refreshSupport` (it honors the nudge — the refresh is
  capability-gated). Sealing against a push-only client is a diagnostics
  blackout; against a pull-without-refresh client, spontaneous pushes rot
  until the client's next edit-triggered pull. Only seal for refresh-capable
  pull clients (Neovim ≥ 0.11 and VS Code qualify).
- Put the `[]` on the `"textDocument/publishDiagnostics"` key specifically,
  never on the `"_"` method wildcard — an empty method-wildcard priorities
  list disables every method's layer walk (hover, completion, formatting, …),
  which has always been `[]`'s meaning there.
- The same resolved value also gates the **debounced pull feed** for the
  method (as before this decision), which carries the #431 host re-sync to
  push-only `_self` host servers — under a seal such a server stops receiving
  debounced text updates and its diagnostics go stale between other
  host-bridge requests. Don't seal a language that relies on a push-only
  `_self` server.
- Apply the seal at startup. A mid-session seal (via
  `didChangeConfiguration`) leaves the last set that actually reached the
  wire frozen in the editor's push namespace until `didClose` (a send
  withheld by the quiet window at seal time is dropped with the gate state,
  so the frozen view can be one window older than the last recorded set) —
  config changes do not republish open hosts (a pre-existing property of
  this pipeline).
- Prior to this decision `priorities = []` on this method gated only the
  pull feed; existing configs using it now also stop the wire sends.

### Path B — Client-initiated pull (retained)

kakehashi keeps answering `textDocument/diagnostic` from the client:

- Fan out a live `textDocument/diagnostic` to every pull-driven downstream,
  transform and aggregate as today.
- For push-driven downstream, serve their cached push slots from Path A via the
  `pushFallback` toggle.
- Merge and respond. The advertised `diagnosticProvider` capability is unchanged.
- The response is deterministically sorted (the same order as Path A's publish,
  #423) and carries a **content-addressed `resultId`** (hash of the serialized
  items — stateless, nothing to invalidate). A re-pull whose `previousResultId`
  matches the recomputed id is answered with an `unchanged` report instead of
  re-shipping the items: on a diagnostics-heavy host the full report is ~1 MB
  (measured: ~2,235 items), and every `workspace/diagnostic/refresh`-induced
  re-pull of a settled set previously re-shipped all of it.

Path A writes the cache; Path B reads it for push-driven servers — one cache,
two readers.

Path B also keeps a downstream pull baseline per exact `(connection key,
downstream document URI)`. A full report with a `resultId` stores that id and
the server-local diagnostics; the next pull sends it as `previousResultId`.
An `unchanged` response reuses those items instead of treating the region as
empty, while advancing the baseline to the response's returned `resultId` for
the next pull. Region items stay in **virtual-local coordinates** and are transformed
with the request's current `RegionOffset` on every full or unchanged response,
so an edit above an otherwise unchanged region re-anchors the cached findings
without invalidating its downstream content lineage. Host items are already in
host coordinates and use the same cache without transformation.

The baseline is discarded on downstream `null`/full-without-resultId, virtual
or host `didClose`, region identity/language replacement, connection respawn,
and a settings replacement affecting that exact connection. A mere geometry shift of the same stable region id
does not discard it: virtual-local storage plus lazy re-anchor is precisely what
makes that reuse correct. Concurrent pulls carry a bridge-local monotonically
increasing request sequence: the later-started request owns the final lineage
regardless of response completion order, so a late older response cannot
overwrite or clear its baseline even when an opaque downstream `resultId`
repeats.

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

Naming note (vs the alternative `pullFallbackEnabled` / `pushFallbackEnabled`): the
`Enabled` suffix is dropped to match the existing bare-boolean `enabled` on
`BridgeLanguageConfig` — the field *is* the toggle. The mechanism stays in the
key (rather than a method-disambiguated bare `fallback`) so a single config line
reads unambiguously out of context.

### Cache model

```
host_uri ──► source ──► server ──► SlotEntry { diagnostics(source-local coords), content_epoch }
```

`source` is a virtual region's URI or the host URI for the `_self` host layer.
The cache is **nested** (`host_uri → source → server`) rather than keyed by a flat
tuple, so the lifecycle's evictions are O(1): a whole `host_uri` on close, a
`source` on region invalidation, a `server` on crash. Several servers can attach
to one source (e.g. two Lua linters on one region) and each pushes independently.
Producing the host publish mirrors the existing staged pull aggregation:

1. **Per-source fan-in** — for each source, combine its servers' slots per the
   method's `strategy` over the **visible walk**: resolve it exactly as pull does —
   expand `priorities`, then truncate by `maxFanOut` (`dispatch.rs`) — and use that
   truncated walk for eligibility and ordering. Cached slots for servers outside
   the visible walk are retained but ignored until a config change re-resolves it.
   Then `concatenated` appends all visible slots, `preferred` elects one (see the
   strategy sections).
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
records `{A}`, `{A, host}`, `{A, host, C}` — a slot is *replaced* by its
source's latest push, never accumulated. (The wire sends are subject to the
quiet window below, so the editor may see `{A}` and then `{A, host, C}`
directly, the intermediate state coalesced into the trailing flush.)

### Versioning and staleness (the crux)

Storing virtual coordinates rather than pre-baked host coordinates makes the
policy clean:

- **Content epoch vs wire version**: gate and elect on a per-source
  **`content_epoch`** — a monotonic counter (or content fingerprint) that advances
  **only when the source's extracted content changes**, shared across all servers
  on that source. This is *not* the per-connection LSP wire version the bridge
  already tracks (`document_tracker` keys version by `(connection, virtual_uri)`
  and bumps per connection), which is not comparable across servers — a restart or
  late open desyncs it. To map a push back to an epoch, kakehashi records a
  nested `connection → source → wire_version → content_epoch` entry (so a
  connection purge drops its subtree in O(1)) **when each
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
- **Geometry-unknown deferral**: between `did_change` clearing the visible tree
  and the off-ingress reparse landing, the regions' current offsets cannot be
  computed. A republish in that window **defers** (publishes nothing, records
  nothing) rather than merging with empty offsets — that would silently drop
  every region push slot and flap the editor between the full and the
  region-less set on each edit cycle (~2 × ~1 MB publishes per keystroke settle
  on a diagnostics-heavy host, plus visible flicker). The reparse loop's
  post-parse republish (gated on the same non-empty-region-slots predicate)
  is the retry that publishes with real offsets, doubly backstopped by the
  pass's debounced pull. A deferral still nudges pull-mode clients: the
  push-origin callers treat it like a change for the coverage bump and
  `workspace/diagnostic/refresh` (the cache did change; the retry itself
  nudges only as degraded-pull recovery — a per-host debt recorded by a pull
  the parse wait failed, consumed once by the post-parse pass or the
  pull-side TOCTOU guard, forced past the coverage gate — the debt proves
  the client holds a non-covering answer that version coverage cannot see —
  but still single-flighted like every refresh). Accepted caveat: a parse
  give-up (timeout,
  parser gone) leaves the tree absent and both retries deferred until the
  next edit — where the pre-deferral behavior published a region-LESS set,
  which was worse.
- **Version gate**: a slot whose epoch lags the source's current `content_epoch`
  is held (not published) until that server re-publishes at the current epoch,
  bounding the wrong-position window to the re-parse gap and self-healing. The virt
  path currently bumps the wire version and re-sends `didChange` for *every* opened
  region on *every* host `didChange` (`did_change.rs::forward_didchange_to_opened_docs`),
  with no content guard; the host path already fingerprints before syncing
  (`host.rs`). This decision requires the virt sync to gain that content guard so
  the epoch advances only on a real content change.
- **Re-merge on epoch bump**: when a source's `content_epoch` advances, kakehashi
  re-merges that `host_uri` *immediately* (the wire send is subject to the quiet
  window), with the now-stale slots held — otherwise nothing would clear the old
  diagnostics until a current-epoch push happens to arrive. The bump itself is
  the trigger, not just incoming pushes.
- **Re-merge on geometry change**: a host edit that shifts a region's position
  without changing its content does *not* advance `content_epoch`, so lazy
  re-anchor alone would leave the previously-published (now mis-placed) host
  coordinates on screen until an unrelated push. The region-offset update (the
  re-parse that moves the region) must itself trigger a re-merge, republishing only
  if the re-anchored host coordinates actually changed. Lazy re-anchor supplies the
  correct positions; this supplies the missing trigger.
- **Host layer**: the `_self` host source uses the identical model keyed on the
  *host document's* content epoch. Push-driven `_self` servers are eagerly opened
  on host `didOpen` (#429) and eagerly **re-synced** on edit at the debounced
  diagnostic cadence (#431) — `eager_sync_host_document_on_servers` runs when the
  debounce fires, so a push-only host server (skipped by the capability-gated
  pull) re-analyzes current text rather than stale text. The host path's
  fingerprint gates the actual didChange; the gate and re-merge rules above apply
  unchanged.

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
- **Winner walk** over the visible walk (expanded-and-truncated `priorities`,
  above):
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
  flattened visible walk** (expanded-and-truncated `priorities`, above) — `Server`
  entries and the `Rest("*")` group each keep their *configured position* (so
  `["*", "a1"]` puts the group first), and the group's members are in server config
  order excluding names listed elsewhere — the order pull already uses, so the
  output stays stable no
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

- A nested `host_uri → source → server` diagnostic cache with lifecycle handling (host
  close, region invalidation, server crash).
- Version/staleness logic returns (version gate + lazy re-anchor); a brief
  hold/wrong-position window remains right after an edit, self-healing on the next
  push.
- The reader must transform and route pushes, interleaved with edits, relying on
  the per-URI ordering guarantee.
- Re-publish is unthrottled at the merge level (the `didChange` debounce is
  gone): a no-op push is suppressed (winner content unchanged), and a chatty
  linter emitting distinct results across separate loop wakes re-merges and
  re-records each time — but the WIRE sends are coalesced by the per-host quiet
  window (see "Wire quiet window"), so the editor sees at most one publish per
  window. To bound the
  cost of a push-happy/misbehaving downstream, the forwarding loop first coalesces a
  drained burst of `publishDiagnostics` by `(connection, uri)` to the latest
  (`coalesce_upstream_batch`, #426), then records each surviving push in a
  barrier-delimited run and publishes the final aggregate once per **resolved host**.
  This second boundary matters for injection-heavy documents: hundreds of distinct
  virtual URIs can update one host, and no editor needs the multi-megabyte
  intermediate aggregate after every region. Every non-publish notification remains
  an order-preserving barrier, so publish/evict and progress ordering stay exact.
- `pullFallback` (default on) keeps a *scoped* host-event pull trigger alive for
  pull-driven servers — the per-event re-pull is removed only for push-driven
  ones, not universally. Setting it `false` drops those servers from the proactive
  path entirely.

### Neutral

- Position transformation and the aggregation strategy are unchanged.
- The client-pull handler is largely unchanged; it now additionally reads the
  push cache for push-driven servers (`pushFallback`).

## Decision–Implementation Gap

**Implemented** (the core push propagation):

- The per-host merge cache (`src/lsp/diagnostic_cache.rs`, `DiagnosticAggregator`)
  and the single `DiagnosticPublisher`
  (`src/lsp/lsp_impl/coordinator/diagnostic_publisher.rs`).
- The host-event pull is unified onto the cache as the `PullLayer` blob (the
  synthetic/debounced feed in `coordinator/diagnostic.rs` /
  `debounced_diagnostics.rs` now republishes through the publisher rather than
  publishing directly).
- Non-scratch `publishDiagnostics` are routed from `reader.rs` into the cache as
  `Region` slots (virtual coordinates, transformed at publish via lazy re-anchor),
  closing #380 for region pushes.
- The `_self` host-layer push source (#421): a push on the real host URI that
  names an open `_self` host-bridged document is cached as a `Host` slot (host
  coordinates) and merged through.
- Host-layer eager-open (#429): on host `didOpen`, the host document is opened
  eagerly on each `_self` host server (`eager_open_host_document_on_servers`), so
  a push-only host server analyzes + pushes on open rather than only after the
  first host-bridged request lazily opens it.
- Host-layer on-edit re-sync (#431): when the debounced diagnostic fires after an
  edit, `eager_sync_host_document_on_servers` re-syncs the host doc to its `_self`
  servers, so a push-only host server (skipped by the capability-gated pull)
  re-analyzes current text. Runs at the debounce cadence (after typing settles),
  not per-keystroke; pull-capable servers fingerprint-dedup the extra sync.
- The `pullFallback` / `pushFallback` config toggles and capability
  classification (#425). `pullFallback` / `pushFallback` are `Option<bool>` on
  `AggregationConfig` (camelCase serde, default `true`), merged field-by-field
  (`merge.rs`). `pullFallback` gates the host-event pull per region/host in
  `prepare_diagnostic_snapshot` (Path A); `pushFallback` folds push-driven
  servers' cached pushes into the client-pull response in `diagnostic_impl`
  (Path B). Servers are classified **live and non-creating** by
  `LanguageServerPool::pull_driven_servers` (static initialize caps + dynamic
  registrations). The pull/push double-count is removed by the publisher's
  `filter_pull_driven_push_slots`, which drops a pull-driven server's push slot
  from the publish whenever a `PullLayer` blob is present, so each server has one
  native source while its slot stays cached (a spontaneous push from a
  pull-driven server still publishes when no `PullLayer` exists).

**Deferred** (staged follow-ups; the code documents each at its site):

- The `content_epoch` version gate and the wire-version↔epoch mapping. Until then
  there is no stale-overwrite guard and a brief wrong-content window after edits
  (lazy re-anchor still keeps *positions* current via the retained pull trigger).
- Per-source strategy fan-in (`preferred` sticky / `concatenated` visible-walk);
  the staged merge concatenates, with HashMap-nondeterministic cross-source order.
  This also subsumes the **interim `pullFallback` dedup limitation**: because
  `PullLayer` is one host-wide blob with no per-server identity,
  `filter_pull_driven_push_slots` triggers on "any `PullLayer` present" rather
  than "this exact server was pulled", so a *mixed* per-region `pullFallback`
  (one region's pull-driven server pulled, a sibling's not) can over-suppress.
  Per-`(source, server)` pull slots remove it.
- Region-invalidation and crash cache eviction.
