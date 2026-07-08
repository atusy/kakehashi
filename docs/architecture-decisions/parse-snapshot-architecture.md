# Parse Snapshot Architecture

**Related Decisions**: [per-document-parse-scheduler](per-document-parse-scheduler.md),
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md),
[injection-token-cache-reuse](injection-token-cache-reuse.md),
[lazy-node-identity-tracking](lazy-node-identity-tracking.md),
[captures-protocol](captures-protocol.md)

## Context

The per-document-parse-scheduler decision moved parsing **off the ingress ticket**: `didChange` applies the text edit, clears the reader-visible tree, and
schedules a coalesced off-ingress reparse. That closed the ingress-latency and
large-paste races it set out to close, but two costs it did not address surfaced
as a user-visible regression against v0.7.0 (which parsed inline on `didChange`):

1. **Per-keystroke read latency on large documents.** `Document::apply_edit_and_seed`
   does `self.tree.take()`: after an edit there is **no servable tree** until the
   off-ingress reparse republishes one. So every tree reader —
   `textDocument/semanticTokens`, `kakehashi/captures`, `kakehashi/node/*`,
   `documentSymbol`, selection ranges — must **block** waiting for the reparse
   (`get_tree_with_wait` on the parse **watermark** via `wait_for_epoch`;
   `ensure_document_parsed` for the captures / node readers).

2. **Cross-document blocking.** A slow reparse on document A stalls semantic
   tokens on an unrelated document B. Instrumentation showed the reader wait on
   a 300-line injection-heavy markdown file spiking to **0.5–1.4 s** per
   keystroke — far past the 200 ms budget. The budget is defeated because the
   reparse's downstream CPU runs **inline on tokio worker threads**:
   `populate_injections` (the injection-cache build from injection-token-cache-reuse,
   O(regions): a per-region node-tracker ULID mint + content hash, hundreds of ms
   for ~900 regions) runs inline in the off-ingress parse loop, and
   `compute_captures` / the `kakehashi/node/*` layer walks run inline in their
   async handlers. On the default `num_cpus`-worker runtime those bursts saturate
   the worker pool, so the tokio timer wheel and `watch` notifications that back
   the 200 ms caps cannot fire, and unrelated documents' handlers cannot be polled.

The primitives to fix this are largely already present — a process-monotonic
`incarnation` per document lifetime, a per-URI parse watermark, and the
`ParseScheduler`'s one-loop-per-document coalescing. What is missing is (1) a
tree that readers can serve **without** waiting for a reparse, and (2) getting
tree-CPU **off** the shared async runtime. The user goal is explicit:
**high-performance parsing, no cross-document blocking, and document lifecycle management independent of parsing** — and large architectural change is invited.

## Decision

Model the document as **versioned inputs** and parsing as a **versioned derived computation**, in the shape salsa / rust-analyzer / gopls use: inputs are the
source of truth and never block on derivation; derivation publishes immutable,
internally-consistent snapshots that readers observe without blocking.

### 1. `Document` holds inputs only

`Document` carries `text: Arc<str>`, `language_id`, `incarnation`, and a
monotonic `content_version` (bumped on every edit, `0` at `didOpen`). It no
longer holds `tree` or `pending_seed`, and `apply_edit` never clears a tree — it
installs the new text and bumps the version. The document lifecycle
(`didOpen`/`didChange`/`didClose`) becomes independent of parsing: it mutates
inputs and returns, never awaiting or gating on a parse. `content_version` is a
**new input-side field** that threads into `DocumentSnapshot` and (as
`parsed_version`) into `ParseSnapshot` and `ComputedCaptures`; adding it and the
`apply_edit` bump is a Stage 1 prerequisite for the version comparisons below.

Two input-side concerns are explicitly **retained**, because they are not parse
gates:

- The per-URI **`edit_lock`** still serializes the non-atomic
  read-old-text → apply-range → persist cycle within `didChange` (and against
  `didClose`). Removing it reopens the stale-base race that pre-clamping panicked
  on in `replace_range`; "mutates inputs and returns" must not be read as
  dropping it.
- **Language detection is split by layer.** The input `language_id` is the
  client-declared LSP `languageId` that `didOpen` records and never changes within
  a lifetime (a genuine relabel is a reopen → new incarnation, which is why the
  CAS in §2 folds language into incarnation). The *parse* additionally re-runs
  content-based detection (its `detect_language(path, text, language_id)`) and
  records the result as the snapshot's own **derived** `language` — which may
  refine the input guess. Derivation never writes that refinement **back** to the
  input (the layering violation the model avoids); the snapshot simply carries
  the more-accurate detected language, and readers use the snapshot's.

### 2. Parsing publishes a versioned `ParseSnapshot`

```
ParseSnapshot { text: Arc<str>, tree: Option<Tree>, language: String,
                parsed_version: u64, incarnation: u64,
                injection_regions: /* region-id + geometry, minted here (see §3) */ }
```

(`tree: Option` so a resolved-but-parser-less outcome is representable — see below.)

A snapshot's `text` is exactly the text its `tree` was parsed from — the two
always agree (the gopls immutable-snapshot property). Snapshots live per-URI in a
single `watch` channel whose value co-locates the current lifetime with the
snapshot:

```
SnapshotSlot { current_incarnation: u64, snapshot: Option<Arc<ParseSnapshot>> }
```

This channel **subsumes** the current `parse_states` and `watermarks` maps, but the
mapping is not one-to-one and must be stated precisely:

- Two distinct predicates replace the old single `has_tree`: **`resolved`** =
  `slot.snapshot.is_some()` (a parse for this lifetime has completed at least once)
  and **`has_tree`** = `slot.snapshot.as_ref().and_then(|s| s.tree.as_ref()).is_some()`. A
  **resolved-but-tree-less** outcome — a parse that completed with no usable tree
  (no parser installed, install failed, or a quarantined crashed grammar), distinct
  from the pre-first-parse `None` — is `resolved && !has_tree`; it advances
  `parsed_version` and releases first-parse waiters (who then fall through to their
  empty / `null` / `ContentModified` paths), which the old boolean `has_tree` could
  not express.
- `parsed_version` is the watermark ticket; `current_incarnation` is the
  per-lifetime guard.
- `ParseState`'s three fields are each accounted for, none silently dropped:
  `has_tree` is **replaced** by the two slot predicates above (not re-homed); the
  **`generation`** (a monotonic parse-run counter that `mark_parse_started` bumps
  and `mark_parse_finished` checks to reject an out-of-order finish) and the
  **`in_progress`** flag **re-home** onto `ParseScheduler`'s per-document state
  (which already owns the parse lifecycle), not onto the read-side slot. Deleting
  `parse_states` is contingent on those two moves.

The store's tree/watermark CAS methods (four tree writes —
`update_tree_if_text_unchanged`, `update_tree_if_text_and_language_unchanged`,
`attach_tree_if_absent`, `set_parse_result_if_text_and_incarnation_unchanged` —
plus the two watermark advances `advance_watermark` /
`advance_watermark_for_incarnation`) collapse to **one publish primitive**,
executed inside `send_if_modified` so the guard and the write are atomic under the
channel's own lock (the co-location is what makes this a single atomic
check-then-act rather than a cross-map TOCTOU against `Document.incarnation`):

> Install `snapshot` iff **both** clauses hold — the incarnation clause is never
> bypassed:
> 1. `snapshot.incarnation == slot.current_incarnation`, **and**
> 2. `slot.snapshot.is_none()` (bootstrap) **or**
>    `snapshot.parsed_version > slot.snapshot.parsed_version` (strict monotonic).
>
> The bootstrap case relaxes only the version compare (clause 2), never the
> incarnation check (clause 1).

- **Incarnation-scoped, strict monotonicity.** The `>` is strict — equal-version
  double-publishes (e.g. a racing open-parse and reparse both at version 0) must
  not swap the `Tree` under an already-issued `result_id` and fire a spurious
  refresh. `didOpen` sets `current_incarnation`; a reopen starts the URI's cell
  fresh at `(current_incarnation = N+1, snapshot = None)` (whether the cell is reset
  in place or recreated is a correctness-irrelevant implementation choice — see the
  isolation bullet). Starting `snapshot` at `None` is what clears the version floor:
  the first post-reopen publish takes the `None` bootstrap branch, so a leftover
  `parsed_version = 5` from the prior lifetime cannot make a fresh `0 > 5` fail
  forever; the incarnation bump alone would not reset the floor. The `None` bootstrap
  still checks `incarnation == current_incarnation` (it is not an unconditional
  early-return), so a straggler publish from lifetime N is rejected against N+1 and
  the version compare is only ever within one lifetime.
- **Language axis** is subsumed by incarnation: every reopen draws a fresh
  incarnation (so a relabel across lifetimes is already rejected), and within one
  lifetime the language does not change. The three-axis edit-path CAS
  (`incarnation && text && language`) therefore reduces to `incarnation && version`.
- **Isolation is per-request re-resolution + incarnation validation, not cell identity.** A `latest_snapshot(uri)` call resolves the *current* cell from the
  store each time and never caches a `Receiver` across requests, and it validates
  `snapshot.incarnation == <live document incarnation>` before serving. To keep that
  a **single `DashMap` lookup** (not a `documents` lookup *plus* a separate
  channel-map lookup), the `SnapshotSlot` `watch` channel lives **on the `Document`
  entry itself** — inputs and slot are one per-URI lifetime object, so one lookup
  yields both the live incarnation and the latest snapshot, and there is one
  authoritative incarnation. This makes cell lifecycle irrelevant to correctness:
  on reopen the store may drop and recreate the cell, and a prior-lifetime parse
  task holding a stale `Sender` clone can still publish — but only into that now
  **detached** old cell, which no current-request reader resolves, so the publish
  is unobservable; and a reader mid-compute holding a lifetime-`N` `Arc<ParseSnapshot>`
  rejects it against the live `N+1` incarnation. The one reader that *does* hold a
  `Receiver` — a first-parse waiter parked on `watch::changed()` — must be woken by
  an **explicit close publish**: `didClose` sets the slot to a terminal state whose
  `current_incarnation` is a **reserved sentinel** (`u64::MAX` — the counter starts
  at 1 and must reserve this value, guarding its `fetch_add` against ever drawing
  it) with `snapshot = None`, rather than relying on the
  channel's senders dropping — stale parse tasks may still hold `Sender` clones that
  keep the channel alive. Setting the sentinel is load-bearing: keeping
  `current_incarnation = N` would let a stale lifetime-`N` publish pass *both* the
  incarnation check (`N == N`) and the bootstrap branch (`snapshot` is now `None`),
  overwriting the terminal state with `Some(_)` and resurrecting the closed document
  for the parked waiter. With the sentinel, that publish fails `incarnation ==
  current_incarnation` (`N != u64::MAX`) and the waiter unambiguously observes the
  closed state and falls through to `null` — the wake the current `wait_for_epoch`
  gets from the watermark sender dropping, made explicit because the snapshot channel
  outlives more clones.

The incremental-parse **seed** re-homes onto `ParseScheduler`'s per-document state
(accumulated `InputEdit`s + the `base_version` they extend). Two obligations,
enforced by co-location today, become explicit scheduler invariants:

- The seed is applied to the snapshot's tree **iff `snapshot.parsed_version ==
  base_version` and `snapshot.tree` is `Some`**; on a version mismatch (a publish
  raced) *or* a tree-less base snapshot (resolved-but-parser-less), there is nothing
  to incrementally seed from, so it parses from scratch — it never applies edits to
  a tree they do not match (the `#348` external-scanner corruption) nor to an absent
  tree.
- A full-text sync **resets** `(pending_edits, base_version)`, so it parses from
  scratch. "Leaves no accumulated edits" is an explicit reset, not an emergent
  property.

A parse pass therefore has a **single version/incarnation-guarded commit sequence**, so no downstream effect ever escapes for a snapshot that lost the CAS:
compute the tree, region map, and tokens **privately** (nothing shared mutated);
revalidate `incarnation == current_incarnation`; run the one publish primitive; and
emit the downstream — `semanticTokens/refresh`, injected-language forwarding,
diagnostic republish, and any shared injection-token-cache write — **only if that exact publish succeeded** (the shared `NodeTracker` is not mutated on this path at
all; see §3), in the order the
per-document-parse-scheduler loop already uses (`populate → mark finished →
downstream`). A rejected publish (a racing edit or reopen advanced the slot) emits
nothing and mutates no shared state. The two Stage-2 stores are **not** written in
one cross-store transaction — that is unnecessary, because they are not co-equal:
the **snapshot publish is the sole commit point and runs first**, and every
downstream effect gates on *its* result. The legacy tree CAS that follows is a
Stage-2-only compatibility shim feeding the incremental seed (which still reads
`Document::tree` until Stage 3 removes it); it is made strict-version so it never
regresses, but if it *loses* a race the only consequence is that the next pass
reseeds from the current snapshot instead of the legacy tree — a self-correcting
perf blip, never a served inconsistency, since readers already read the snapshot,
not the legacy tree. So no reader-visible divergence can arise from the ordering.

### 3. Reader contract — non-blocking, three classes

Readers call a wait-free `latest_snapshot(uri)` (a `watch` borrow) and never parse
inline. A reader serves the **latest completed snapshot**; when
`parsed_version < content_version` it is stale by however many edits the scheduler
coalesced since — one under light typing, but **potentially several** under
sustained typing (per-document-parse-scheduler deliberately coalesces, so the
watermark can trail the input indefinitely under a fast enough edit stream). The
refresh path re-drives the client each time a fresher snapshot lands; the model
does not promise a bounded edit-lag without the fair-admission backpressure §4
defers.

A handler resolves `latest_snapshot(uri)` **once**, at entry, and threads the
resulting `Arc<ParseSnapshot>` (a refcount clone) into any parallel fan-out —
never calling `latest_snapshot` per fan-out task, which would hammer the store's
`DashMap` shard from every worker. The single up-front resolution also pins one
consistent snapshot for the whole request; a publish landing mid-fan-out is
observed by the *next* request, not by half of this one.

Region identity is the load-bearing subtlety, and it forces a clean split. The
shared `NodeTracker` (lazy-node-identity-tracking) is a **mutable, edit-shifted**
index: it is adjusted synchronously on `didChange` to keep `kakehashi/node/*`
references stable across edits, so it tracks `content_version`. A snapshot's tree
tracks `parsed_version`. Minting region ids by mutating that shared tracker from a
parse pass — at `parsed_version`, into an index the live document has already
edit-shifted to `content_version` — cannot be made atomic without holding
`edit_lock` across the whole parse, and even a version-gated mutation leaves the
"where do the ids come from when the gate fails?" question undefined. Mutating a
shared, edit-shifted identity index off the derivation path is fundamentally at
odds with the snapshot model.

The decision therefore **separates three concerns that today all ride the tracker ULID**, so none needs a shared, edit-shifted mint off the parse path:

- **Within-snapshot enumeration** — the id `populate` puts in `injection_regions`
  is a **document-order ordinal** (the region's index in the injection-query match
  order over the snapshot's tree), paired with the region's byte geometry. It is
  deterministic from the snapshot's own tree and unique within the snapshot even
  for byte-identical regions (the ordinal disambiguates), so `(URI, region_id)`
  cannot alias two regions of one snapshot. It is **not** a cross-edit-stable handle
  and is not claimed to be — an edit that inserts a region renumbers ordinals.
- **Cross-edit token reuse** (the injection-token-cache-reuse benefit): the cache
  is already content-validated at read (its entry carries `validity_hash` = content
  ⊕ language, and the settings `generation`), but its *key* is today the tracker
  ULID. The change is to **drop `region_id` from the key** — key on the content
  identity (implemented as `(uri, validity_hash)`, the content ⊕ resolved-language
  fold), keeping the settings `generation` as entry metadata checked at read, not
  as a key component. The key **must stay stable
  across edits** for reuse to work, so it must not include anything that turns over
  per edit; the `generation` here is the **config/settings epoch** (bumped only on a
  settings reload, the #530 discipline — *not* the per-edit parse-run counter), so a
  byte-identical region reuses across edits and is invalidated only on a real config
  change. Two snapshots' byte-identical regions thus reuse tokens without any stable
  id. That in turn reworks **eviction** — and this companion decision is now
  **implemented**: the key is `(uri, validity_hash)` (content ⊕ resolved
  language, one shared fold), spatial `invalidate_for_edits` is deleted
  (content addressing self-invalidates: an edited region misses under its new
  hash, and an unrelated edit cannot touch a region's entry at all — the
  "typed region preserves reuse" property holds *without* the interval
  index), and the growth bound is a per-populate **live-hash sweep** run
  inside the mint-epoch commit (a stale pass sweeps nothing). The live set
  comes from the discovery's own per-region cache identities so the sweep and
  the store/read path can never disagree on the fold. Free extras: duplicate
  fences share one entry, and an undo back to previous content is a hit.
- **Cross-edit bridge / virtual-document identity** — the stable handle a
  downstream language server's virtual document is keyed by — **remains the shared `NodeTracker` ULID**, a live-position (`content_version`) concern. Because regions
  are only *discovered* by a parse, minting still originates from a parse pass, but
  as a distinct **reconciliation step**, not an inline per-region mint off the
  stale-read path. **Implemented** (`NodeTracker::mint_batch_if_unshifted`):
  the pass latches the tracker's (shift generation, cleanup epoch) at entry,
  derives its region geometry, then maps that geometry to tracker ULIDs in one
  batch — reusing an existing id by position, minting a fresh one for a
  genuinely new region — with the latch re-checked under the tracker entry's
  exclusive lock, the same lock the `didChange` edit-shift takes. The latch is
  necessary but not sufficient on its own: `didChange` shifts the tracker
  BEFORE it bumps `content_version`, so the pass captures the latch **and**
  validates liveness + lifetime + currency (incarnation and
  `parsed_version == content_version`) in one critical section under
  `edit_lock` (`populate_injections_on_pool`) — atomic against the shift/bump
  pair — and everything landing after that section is what the latch re-check
  inside the batch mint/commit catches. A batch that passes is *correct-at-birth* — a later edit
  shifts its ids like any live entry — so there is no purge machinery; a batch
  the latch refuses minted **nothing**: the stale pass withholds every
  region-id-bearing product and defers identity to the next current pass, the
  tracker meanwhile kept live by the ordinary `didChange` edit-shift
  (`apply_input_edits`), so the bridge always resolves against a
  `content_version`-consistent index. A *stale-tree read* still never mints or
  mutates it — only the latch-gated reconciliation does. So `populate` splits in
  two: derive the snapshot-owned geometry (always), and (only while unshifted)
  reconcile tracker ULIDs. The captures walk applies the same primitive
  per layer — one latch-gated batch per visited layer instead of one tracker
  lock acquisition per capture.

So a stale-tree reader reads self-consistent ordinals + geometry from its own
snapshot and touches no shared identity index (the mint-race and the
undefined-id-on-gate-failure both vanish); token reuse is content-keyed; and the
one identity that genuinely needs cross-edit stability (the bridge's) keeps the
tracker that already provides it. This ADR commits only to *not* mutating shared
identities from a **stale** parse pass or any **stale** read path — the
latch-gated reconciliation above is where a parse mints them, and the
currency-gated inline readers (the Stage-2 remainder below) still mint on
their current-snapshot path.

Any reader that must resolve against **live** positions is position-critical
(below).

- **Serve-current (parked wait), refresh as backstop** — `semanticTokens`
  full/delta. The handler parks (racing the client's `$/cancelRequest`) until
  the latest snapshot is **current** (`parsed_version == content_version`),
  then computes against that snapshot's consistent
  `(text, tree, injection_regions)`; caches key off the snapshot's
  `(text, parsed_version)` so nothing is poisoned.

  Stage 2 originally shipped this reader as *serve-stale + refresh* (answer
  from the latest completed snapshot immediately; let the parse loop's
  `workspace/semanticTokens/refresh` re-drive the client once a fresher one
  lands). Live-editor evidence overturned that: the dominant client
  (`vim.lsp.semantic_tokens`) stamps a response with the buffer version **at
  request time** and draws it as soon as that version matches the live buffer
  (extmarks placed with `strict = false`), so tokens computed for older text
  render visibly misplaced on *unchanged* lines until the refresh round-trip
  converges — highlight corruption, not mere latency. While the server stays
  silent instead, the editor keeps its previous tokens as extmarks that shift
  naturally with edits: the acceptable degradation. Serving stale is strictly
  worse than parking for every client that draws responses against the live
  buffer, and parking is affordable because the wait is bounded by parse
  completion (every edit's parse resolution publishes), not by typing — a
  superseding request (the client re-requests per edit) or `$/cancelRequest`
  releases a parked handler early.

  On the settle backstop expiring (pipeline pathologically behind), the
  handler rejects with `ContentModified` and the parse loop's settled+stale
  refresh gate re-drives the client once the parse lands. The refresh trigger
  stays at the **parse-loop** publish point, **not** in the `didChange`
  handler — `didChange` deliberately does not emit refresh because a
  synchronous client (vim-lsp on Vim) cannot answer a server request while
  processing a notification; emitting from the off-ingress loop, after the
  notification returns, avoids that reentrancy. Under serve-current the gate
  rarely fires (a parked request records the served version at settle); it
  remains for the backstop path and non-edit token changes. The `CancelToken`
  bail stays narrowed to reclaiming a superseded compute's CPU (§4).
- **Serve-stale, passively refreshed** — whole-document, no-position reads:
  `documentSymbol`, `documentColor`, `documentLink`, `foldingRange`, `codeLens`
  (the `whole_document_preferred_fan_out` family), and pull-mode
  `textDocument/diagnostic` (the `virt_enabled` branch that calls
  `ensure_document_parsed`). `did_save`'s synthetic-diagnostic effect is likewise
  passive (a notification, not a request — its *diagnostics* may trail a snapshot,
  self-healing on republish). No server→client refresh exists for these and adding
  one is out of scope; they serve the latest snapshot and self-correct on the
  client's **next** request (re-requested on redraw/scroll, not held live). The
  staleness window is one or more edits (unbounded under sustained typing) but
  non-visual-jarring; this is the deliberate, user-sanctioned relaxation.
- **Staleness-reject** — every **position/range** reader, because its
  coordinates are authored against the **live** text: `semanticTokens/range`,
  `kakehashi/node/*`, `selectionRange`, `formatting`, `rangeFormatting`,
  `captures/range`, **and the ~16 position/range bridge-context requests** that
  resolve an injection region before forwarding to a downstream server (hover,
  definition, references, declaration, typeDefinition, implementation, rename /
  prepareRename, completion, signatureHelp, documentHighlight, linkedEditingRange,
  moniker, on-type formatting, inlayHint, colorPresentation — the
  `bridge_context` `ensure_document_parsed` site). A stale tree cannot answer them,
  and captures/node/bridge additionally resolve against the live `content_version`
  tracker. The rejection signal differs by protocol contract:
  - The LSP requests return **`ContentModified`** (-32801) when
    `content_version > parsed_version`, and never serve a wrong-position result.
    The spec does not mandate client auto-retry, so the reject is split by trigger:
    *implicit/background* requests (hover, signatureHelp, documentHighlight, inlayHint,
    …) reject immediately and get their answer on the client's next natural request.
    *Explicit, user-initiated, infrequent* actions (`formatting`, `rangeFormatting`,
    `rename`/`prepareRename`, and `selectionRange` — keyboard-triggered
    expand/shrink) take a **brief bounded wait** (the reader's only permitted wait
    besides first-parse) for the in-flight parse to land before falling back to
    `ContentModified` — because a silent no-op on an action the user consciously
    triggered is jarring, and the wait is affordable exactly because these are not
    per-keystroke.
  - `kakehashi/captures/range` uses `null` where the LSP requests use
    `ContentModified` — `null` is the re-sync signal captures-protocol already
    defines ("on null, call full again"), and a JSON-RPC *error* would violate that
    contract. `captures/full` and `full/delta` take the **serve-current parked
    wait**, like `semanticTokens`. This revises the original serve-stale choice,
    which was motivated by avoiding a `null` busy-spin (edit → stale → `null` →
    re-request → …): live-editor evidence (capture 01KWSECM…) showed serve-stale
    *creating* exactly that spin one layer down — every stale walk completed its
    full compute, was then voided by the lineage still-current gate, answered
    `null`, and triggered a re-request that walked the next trailing snapshot
    (41 walks in ~15s, most doomed, saturating the compute pool and queueing the
    semantic delta ~10s behind them). Parking costs a `watch` subscription,
    releases on the parse's own publish (or the client's cancel), and each
    settle then walks once per kind with lineage installing. `null` remains for
    what the protocol means by it: no snapshot yet (pre-first-parse), the settle
    backstop, and a delta with no lineage. When a served `full` installs
    lineage, it carries the computed `parsed_version` and — under `edit_lock` —
    stores it **only if `incarnation` and the current `content_version` still
    match**, else stores nothing and returns `null`, so the lineage never
    records matches for a text the client no longer has (under serve-current
    this guards the resolve→delivery window).

Three bounded waits remain, all deliberate: the **first parse after `didOpen`**
(the snapshot is `None`; it waits briefly on `watch::changed()` then serves each
reader's empty fallback — protocol `null` for captures/node, an empty
`SemanticTokens` for the token handlers — the same decoupling
parse-decoupled-document-lifecycle applies to its host tier),
the **explicit-action wait** above (`formatting`/`rename`), and the
**serve-current token wait** (`semanticTokens` full/delta — parked, not
polling; bounded by parse completion and released early by supersede/cancel).
No other *per-keystroke* read ever waits.

*(`kakehashi/node/*` stays staleness-reject: a node request resolves to a
specific live-position node, so a stale-position answer is wrong, not merely
late — and it has no per-keystroke re-request to heal it. Serving it stale
would need a per-snapshot tracker view, which this decision does not commit
to; parking it like the token readers is possible but unmotivated — its
implicit-trigger cadence means the client's next natural request heals.)*

### 4. All tree-CPU runs on one bounded compute pool

A single dedicated `rayon` pool sized `available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(2).max(1)`
— strictly below `available_parallelism` whenever there are ≥3 cores, and reserving
at least one core for the tokio workers + timer driver on 2-core machines (on a
1-core host no isolation is achievable and the pool degrades to shared time-slicing)
— runs **all** synchronous tree work: the parse, `populate` (snapshot
ordinal/geometry derivation + content hash, and — only at current version — the
tracker reconciliation, §3), the `compute_captures` / node layer walks, and the
semantic-token injection fan-out (folded off the process-global Rayon pool into
this one). Async
handlers hand a whole work-unit to the pool (bridged by a `oneshot`) and `await`
the result, so no tree-CPU ever executes on a tokio worker. `DocumentParserPool`'s
guard becomes a sync mutex (`std`/`parking_lot`) since acquisition now runs on pool
threads that cannot `.await`.

The guarantee this design **actually** makes is narrow and unconditional: **the async runtime is never blocked by synchronous tree-CPU** — leg (1), no tree-work on
the tokio workers, so B's handler and the timer driver are always pollable and the
timeouts that were being defeated now fire. It does **not** by itself guarantee
"no cross-document starvation" in the strong sense: the pool has finite threads,
Rayon does not preempt, `ParseScheduler` coalescing caps *parse submissions* per
document but not fan-out task count, task duration, or the number of documents, and
a fan-out on A can still occupy every pool thread ahead of B's work. What Stage 1
buys is that this contention is confined to the compute pool and no longer freezes
unrelated async I/O, diagnostics, or the request loop. Turning "no starvation" into
a real guarantee requires **explicit fair admission** — a focused-document priority
queue plus a per-document in-flight concurrency cap — which this decision names as
required follow-on work, not something the bounded pool delivers on its own.

**Cooperative cancellation is the complement to the pool, and it has already landed on the current architecture.** The pool isolates tree-CPU but cannot stop a
*superseded* compute: Rayon does not preempt, and dropping the `oneshot`-bridged
future leaves the work-unit running to completion — wasting a pool thread that is
now scarcer than the process-global Rayon pool it replaces. The merged `CancelToken`
(`src/cancel.rs`: an `Arc<AtomicBool>` flipped by `SemanticRequestTracker` on
supersede, `$/cancelRequest`, or `didClose`, which the compute polls at coarse
checkpoints — the injection discovery loop and the per-region fan-out — and bails)
closes exactly that gap. It **composes with** this design rather than competing, so
it is carried forward, not re-derived: the token is an architecture-independent
primitive, **re-homed at Stage 1 as a cancellation hook on the bounded-pool work-unit (the `oneshot` bridge) contract**. Keep the split explicit — §2's terminal
`SnapshotSlot` + incarnation rejection guarantees *correctness* on close/supersede
(a stale or cross-lifetime result is never served), while the `CancelToken` is the
orthogonal *CPU-reclamation* mechanism (a superseded compute stops burning a pool
thread); both are wanted, and neither subsumes the other.

### 5. Staged rollout

Delivered in independently shippable, reviewable, measurable stages that stay
inside the existing safety contracts at each step:

- **Stage 1 — tree-CPU off the async workers onto the bounded pool.** Move
  `populate` (all call sites) and the captures / node layer walks off the tokio
  workers; introduce the bounded pool and route parse + populate + walks + fan-out
  through it; convert `parser_pool` to a sync mutex. **All** `parse_with_pool` call
  sites move onto the pool, including the reader on-demand parse fallbacks
  (`try_parse_and_update_document`, `selection_range_impl`) — this is required, not
  optional: converting `parser_pool` to a sync mutex while a reader still parses
  *inline on a tokio worker* would let that worker synchronously block on the mutex
  a Rayon thread holds, reintroducing exactly the block Stage 1 removes. Routing
  those fallbacks through the pool means the sync mutex is only ever acquired on a
  Rayon thread. The reader *contract* is otherwise unchanged (a fallback still
  blocks its own request on the parse — Stage 2 is what makes reads non-blocking);
  Stage 1 only relocates the CPU. Delivers *the async runtime is never blocked by
  tree-CPU* (killing the cross-document freeze) and most of *high-performance
  parsing*; fair admission on the compute pool is a later stage. The
  clear-tree-on-edit contract is untouched, so the stale-tree guarantee is not at
  risk; a further obligation is that `populate` is **awaited** (not detached),
  preserving the `populate → finish` ordering the injection-map invalidation
  depends on.
- **Stage 2 — versioned snapshot reads.** Introduce `SnapshotSlot` + the `watch`
  channel + `latest_snapshot`. **The parse loop dual-writes under one guard**: the
  legacy tree CAS (the incremental seed still reads `Document::tree`/`pending_seed`,
  which Stage 3 removes) is made strict-version like the publish, both writes run
  under the same `(incarnation, parsed_version)` guard, and the **snapshot publish is the sole commit point** — all downstream (refresh, forwarding, diagnostics,
  shared cache writes) gate on the *publish* result, never the legacy CAS (§2). So
  `latest_snapshot` retains a servable tree across an edit's `tree.take()`; the two
  stores may transiently sit at different versions (a lost legacy CAS just reseeds
  next pass), but no **reader** sees the difference because every reader reads the
  snapshot, not the legacy tree. `populate`'s split into geometry derivation and
  the latch-gated tracker reconciliation (§3) is **already in**. Take the
  grammar auto-install off the read handlers (`compute_captures` no longer triggers
  `ensure_injection_languages_loaded_for_document` inline): the parse loop
  *detects* a missing injected grammar and spawns the install as a **detached
  off-ingress task**. The download + compile (seconds) runs entirely on that task,
  **never on the ingress path and never under `edit_lock`** — only the O(1)
  detect-and-spawn is near the lock, and even that need not hold it. The region is
  re-minted on a later reconciliation once the grammar lands. Only the fast
  latch-gated tracker-mint itself runs inside the tracker's shifting lock. Convert
  `semanticTokens` full/delta and `captures/full` to the serve-current parked
  wait (the latter reserving
  `null` for no-snapshot / no-lineage / the settle backstop); the whole-document
  passive-refresh family (`documentSymbol`/`documentColor`/`documentLink`/
  `foldingRange`/`codeLens`/pull-`diagnostic`) to serve-stale; the position/range
  family — `semanticTokens/range`, `node/*`, `captures/range`,
  `formatting`/`rangeFormatting`, the bridge-context requests, **and
  `selectionRange`, whose inline `parse_with_pool` + `update_document` must be
  replaced by `latest_snapshot`** — to `ContentModified` (or `null` for
  `captures/range`). Removes `get_tree_with_wait` /
  `wait_for_epoch` / `ensure_document_parsed` **and** the inline-parse fallbacks
  (`try_parse_and_update_document`, `selection_range_impl`) — closing the
  resurrection vector. Delivers *instant reads* and *lifecycle independent of
  parsing*. Without the dual-write this stage would reproduce the empty-after-edit
  regression, so it is mandatory here, not deferred.
- **Stage 3 — consolidation.** Remove `Document::tree` / `pending_seed` (the seed
  now lives on the scheduler), collapse the CAS methods into the one publish
  primitive, delete the watermark waits, and prune the now-superseded passages
  from per-document-parse-scheduler (its tree-clear-on-edit and watermark/epoch
  sections) per delete-on-supersede — done **here**, when the behavior actually
  changes, not before. Per the repo's structural-vs-behavioral separation
  guideline, the **pure dead-code / doc-pruning** parts of this stage (dropping the
  now-unused fields, deleting the superseded ADR passages) land as **separate PRs**
  from the behavioral collapse (the CAS→publish merge, scheduler seed re-homing),
  each shipped only after its behavioral prerequisite is in.

## Considered Options

- **Keep the scheduler, only move `populate` to `spawn_blocking`.** Rejected as the
  endpoint: it removes the cross-document CPU starvation but not the per-keystroke
  read latency — with the tree still cleared on edit, readers keep blocking on the
  reparse (or fall into an on-demand full parse). And `spawn_blocking` **alone**
  trades HOL-blocking for CPU oversubscription on the default 512-thread blocking
  pool, so it must be paired with a bounded pool regardless. The *idea* — get the
  CPU off the async workers — is what **Stage 1** adopts, but via the **bounded Rayon pool** of §4, **not** raw `spawn_blocking`; and Stage 1 is a step toward the
  snapshot destination, not the destination itself.
- **Serve the pre-edit / seeded tree immediately without versioning (naive stale-serve).** Rejected. It is crash-safe (the `#348` hazard is a
  parse-seed/external-scanner issue, not a query-cursor issue, and the read paths
  are already stale-offset-hardened), but readers **write** persistent caches. The
  whole-document token cache (in `semantic_cache` / `cache`, keyed by text hash) is
  closed by keying off the snapshot's text. The **injection-token cache** and
  the **node-tracker ULIDs** are *not* closed by text-keying — they are closed
  by the content-addressed token-cache key and the §3 latch-gated mint
  reconciliation (both now in) so a stale read consumes stale-but-consistent
  ids and mints nothing. Naive stale-serve does neither and poisons both
  permanently (no generation bump, no refresh today).
- **A text-owning parse actor (per-document-parse-scheduler's Option 4).** Still
  rejected for the reason recorded there: reads bypass the owner, so owning the
  text buys nothing while resurrection-safety must still guard every reader
  fallback. This decision *reuses* the scheduler and its epoch rather than
  replacing them with an actor.
- **Block all readers until fully current (status quo).** Rejected: it is the
  behavior producing the 0.5–1.4 s waits — those waits were *synchronous*
  (inline parses and lock convoys on the request path, stalling unrelated
  documents). The `semanticTokens` full/delta reader has since moved back to a
  **parked** current-wait (§3) because live-editor evidence showed serve-stale
  corrupting drawn highlights; the crucial difference from the status quo is
  that the park is a cheap `watch` subscription bounded by the off-ingress
  parse (§4 keeps the runtime pollable), released early by supersede/cancel,
  and never blocks other documents or other readers.

## Consequences

### Positive

- Document lifecycle is fully decoupled from parsing: `didChange` never awaits a
  parse, and no read *blocks* the runtime on a parse. The bounded waits are
  deliberate: the first-parse wait after `didOpen` (no snapshot yet), the
  explicit-action wait (`formatting`/`rename`) that trades a jarring no-op
  for a short pause on a user-triggered command, and the serve-current token
  park (§3) — a `watch` subscription released by the parse's own publish,
  chosen over serve-stale because answering from a trailing snapshot corrupts
  what the editor draws.
- The async runtime is never blocked by tree-CPU: with all synchronous tree work
  on the bounded pool, a slow parse on one document cannot freeze the request loop,
  timers, diagnostics, or another document's async handlers — the specific defect
  behind the 0.5–1.4 s cross-document waits. (Full *no-starvation* on the compute
  pool itself additionally needs the fair-admission follow-on §4 names.)
- High-performance reads: highlight requests return against the latest snapshot
  immediately instead of waiting hundreds of ms for a reparse + populate.
- The scheduler ADR's residual **reader-fallback resurrection vector is eliminated at Stage 2**: once every reader routes through wait-free `latest_snapshot` and no
  reader parses inline, there is no reader path that can re-insert a document a
  `didClose` removed. (It is *not* closed by Stage 1, which leaves the current
  inline-parse fallbacks — `try_parse_and_update_document`, `selection_range_impl` —
  in place; those are the vector, and they are removed in Stage 2.)
- Latent language-detection staleness is closed as a side effect:
  `Document::language_id` remains the client-provided LSP `languageId` for the
  document's lifetime, while `ParseSnapshot.language` carries the *parse's*
  detected language, so readers see the refined language without a re-`didOpen`.
- State and coupling shrink: two `watch` maps collapse to one, six store CAS /
  watermark methods to one publish primitive, and the `pending_seed` invariant
  surface on `Document` is deleted (favorable on the State > Coupling > Complexity
  > Code ordering the repo optimizes for).

### Negative

- Highlight-class results trail the input until a fresh snapshot publishes.
  `semanticTokens` full/delta **parks** for the current snapshot (silence keeps
  the editor's shifted extmarks intact — see §3) and falls back to
  `ContentModified` + the settle refresh only past its backstop; `captures/full`
  and `full/delta` take the same parked wait (§3 — revised from the original
  serve-stale on live-editor evidence), answering `null` (their protocol
  re-sync signal) past the backstop; `documentSymbol`/`documentColor`
  self-correct only on the client's next natural request (no active refresh
  added).
- Staleness-reject requests (`kakehashi/node/*`, range requests, formatting) return
  `ContentModified` during the reparse window (`captures/range` returns `null`, its
  protocol re-sync signal). The LSP spec does not mandate retry on `ContentModified`,
  so on a client that does not re-request (e.g. Neovim's built-in client for
  formatting) that keystroke's request **no-ops** until the next natural request;
  `formatting`/`rename`/`selectionRange` mitigate with the brief bounded wait.
  `captures/full` is *not* in this class — it takes the serve-current parked
  wait (§3). Keeping `node/*` staleness-reject
  rather than serve-stale is the cost of not forking a per-snapshot tracker view now.
- The `#342`/`#374` stale-tree guarantee (a reader observes at least its own edit)
  is relaxed to "a reader observes a *consistent* snapshot that may trail the input
  by one or more edits, and knows it via the version tag." Correctness now rests on
  the version tag + refresh rather than on the reader having waited.
- Folding the semantic fan-out from the process-global Rayon pool (all cores) into
  the bounded `available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(2).max(1)` pool caps
  single-document tokenization throughput (e.g. ~2 of 4 cores for one huge file) in
  exchange for
  cross-document isolation. Acceptable given the isolation goal, but a real
  single-doc regression to measure.
- Migration touches wide surface: `Document`, `DocumentStore`, `ParseScheduler`,
  the parse coordinator, `parser_pool` (Stage 1 sync-mutex conversion), and the
  ~17 `ensure_document_parsed` call sites. Each invariant from the scheduler and
  lifecycle decisions (edit_lock ordering, captures lineage, diagnostic republish
  order, incarnation CAS on reopen) must be re-proved against the snapshot model.

### Neutral

- `didClose` publishes a terminal `SnapshotSlot` with the sentinel
  `current_incarnation = u64::MAX` and `snapshot = None` (§2) — not merely dropping
  the channel, since stale parse-task `Sender` clones can keep it alive — which
  wakes any reader parked on the first-parse `watch::changed()` and rejects any
  stale-lifetime publish; a reopen starts the cell fresh at the next incarnation. A
  wait-free `latest_snapshot` borrow racing close+reopen may hand back the prior
  lifetime's snapshot momentarily, but the reader's mandatory
  `snapshot.incarnation == <live incarnation>` check (§2) rejects it, so a
  cross-lifetime snapshot is discarded rather than served — it degrades to the
  empty/`null` path, not stale data.
- The `ParseScheduler` (one loop per document, coalescing, panic re-arm),
  `incarnation`, the `edit_lock`, and the incremental-seed concept are retained —
  re-homed, not removed.
- The bounded pool's size and the focused-document priority are a single knob;
  strict fairness (a per-document concurrency cap) can be added later without
  changing the model.

## Decision–Implementation Gap

**Stage 1 is implemented**: the bounded compute pool (`src/compute_pool.rs`)
runs the parse, `populate`, the captures/node layer walks, and the semantic
fan-out (its `par_iter` schedules on the bounded pool, not the process-global
one); `parser_pool` is a sync mutex acquired only on pool threads; all
`parse_with_pool` call sites — including the former reader fallbacks — route
through the pool. The merged `CancelToken` is re-homed as the pool work-unit's
dequeue hook (§4).

**Stage 2 is implemented in its core**: `content_version` on `Document`;
`ParseSnapshot` published into the `SnapshotSlot` `watch` cell on the
`Document` entry through the §2 publish primitive (bootstrap/strict-version/
incarnation guard, `u64::MAX` close sentinel, reserved in `next_incarnation`
and refused by the guard itself); the parse loop dual-writes on every
resolution path and emits `semanticTokens/refresh` at its publish point;
`semanticTokens` full/delta serve-current via the parked wait (initially
shipped serve-stale; revised on live-editor evidence — see §3) and
`captures/full` likewise serve-current (initially serve-stale; revised after
its stale walks proved guaranteed-null compute waste — the lineage store
keeps the §3 request-entry version guard for the resolve→delivery window); `semanticTokens/range` and the
bridge/formatting/node families staleness-reject (`ContentModified`, or the
protocol-appropriate `null`); `formatting`, `rangeFormatting`, and
`selectionRange` take the explicit-action bounded wait (the two formatting
verbs share the treatment as well as the `textDocument/formatting`
configuration key); and **every** reader inline-parse fallback is
removed — `get_tree_with_wait`, `wait_for_epoch`, and the on-demand parse in
`ensure_document_parsed` are gone, closing the resurrection vector.

Two Stage-2 pieces remain deliberately open:

- The **reader-side remainder of the §3 identity split**. The writer half is
  in: the tracker reconciliation is the latch-gated batch mint
  (`mint_batch_if_unshifted`, see §3 above) in `populate` and the captures
  walk, and the injection-token-cache key rework is content-addressed. What
  stays open is the readers whose *inline* region resolution (the
  non-prebuilt fallback) still mints tracker ULIDs — the whole-document
  fan-out family and the bridge-context requests — which therefore still
  require a **current** snapshot (bounded wait, degrade on staleness) instead
  of serving stale: a stale read must never mint, and currency remains the
  way to guarantee that until those readers consume snapshot-owned region
  identity instead of minting.
- Several notification-path consumers (pull diagnostics, `did_save`,
  `documentSymbol`/`documentColor`) still read the legacy store tree after the
  bounded current-wait; they migrate to `latest_snapshot` with the Stage-3
  consolidation.

**Stage 3 is not started**: `Document::tree`/`pending_seed` removal, the
CAS-methods → publish-primitive collapse, watermark deletion, and the
delete-on-supersede pruning of the overtaken per-document-parse-scheduler
passages all happen there (structural PRs separate from behavioral, per §5).
