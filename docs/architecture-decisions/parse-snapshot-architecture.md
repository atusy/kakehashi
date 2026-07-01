# Parse Snapshot Architecture

**Related Decisions**: [per-document-parse-scheduler](per-document-parse-scheduler.md),
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md),
[injection-token-cache-reuse](injection-token-cache-reuse.md),
[lazy-node-identity-tracking](lazy-node-identity-tracking.md),
[captures-protocol](captures-protocol.md)

## Context

The per-document-parse-scheduler decision moved parsing **off the ingress
ticket**: `didChange` applies the text edit, clears the reader-visible tree, and
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
**high-performance parsing, no cross-document blocking, and document lifecycle
management independent of parsing** — and large architectural change is invited.

## Decision

Model the document as **versioned inputs** and parsing as a **versioned derived
computation**, in the shape salsa / rust-analyzer / gopls use: inputs are the
source of truth and never block on derivation; derivation publishes immutable,
internally-consistent snapshots that readers observe without blocking.

### 1. `Document` holds inputs only

`Document` carries `text: Arc<str>`, `language_id`, `incarnation`, and a
monotonic `content_version` (bumped on every edit, `0` at `didOpen`). It no
longer holds `tree` or `pending_seed`, and `apply_edit` never clears a tree — it
installs the new text and bumps the version. The document lifecycle
(`didOpen`/`didChange`/`didClose`) becomes independent of parsing: it mutates
inputs and returns, never awaiting or gating on a parse.

Two input-side concerns are explicitly **retained**, because they are not parse
gates:

- The per-URI **`edit_lock`** still serializes the non-atomic
  read-old-text → apply-range → persist cycle within `didChange` (and against
  `didClose`). Removing it reopens the stale-base race that pre-clamping panicked
  on in `replace_range`; "mutates inputs and returns" must not be read as
  dropping it.
- Content-based **language detection** stays an input concern: `didOpen`/the
  language-detect path writes `language_id` on the `Document`. Derivation reads
  it and copies it into the snapshot; derivation never writes back to the input
  (that would be the layering violation the model exists to avoid).

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

This channel **subsumes** the current `parse_states` (`has_tree`) and `watermarks`
(`(incarnation, ticket)`) maps: `snapshot.is_some()` is has-tree; `parsed_version`
is the watermark ticket; `current_incarnation` is the per-lifetime guard.

A **resolved-but-tree-less** outcome — a parse that completed with no usable tree
(no parser installed, install failed, or a quarantined crashed grammar), distinct
from the pre-first-parse `None` — must still release readers parked on the
first-parse wait. It carries its own resolved-version marker: `ParseSnapshot.tree`
is `Option` (`None` = resolved, no tree), so `snapshot.is_some()` means "resolved"
and the inner `tree` distinguishes "resolved with a tree" from "resolved, none
available"; a tree-less snapshot advances `parsed_version` like any other and
readers fall through to their empty / `ContentModified` paths.

The store's tree/watermark CAS methods (four tree writes —
`update_tree_if_text_unchanged`, `update_tree_if_text_and_language_unchanged`,
`attach_tree_if_absent`, `set_parse_result_if_text_and_incarnation_unchanged` —
plus the two watermark advances `advance_watermark` /
`advance_watermark_for_incarnation`) collapse to **one publish primitive**,
executed inside `send_if_modified` so the guard and the write are atomic under the
channel's own lock (the co-location is what makes this a single atomic
check-then-act rather than a cross-map TOCTOU against `Document.incarnation`):

> Install `snapshot` iff `snapshot.incarnation == slot.current_incarnation` **and**
> `snapshot.parsed_version > slot.snapshot.parsed_version` (strict; `None` →
> first snapshot is the sole bootstrap exception).

- **Incarnation-scoped, strict monotonicity.** The `>` is strict — equal-version
  double-publishes (e.g. a racing open-parse and reparse both at version 0) must
  not swap the `Tree` under an already-issued `result_id` and fire a spurious
  refresh. `didOpen` sets `current_incarnation`; a reopen **installs a fresh
  `SnapshotSlot` (next incarnation, `snapshot = None`)**. Resetting `snapshot` to
  `None` is what clears the version floor — the first post-reopen publish takes the
  `None` bootstrap branch, so a leftover `parsed_version = 5` from the prior
  lifetime cannot make a fresh `0 > 5` fail forever; the incarnation bump alone
  would not reset the floor. The `None` bootstrap still checks
  `incarnation == current_incarnation` (it is not an unconditional early-return), so
  a straggler publish from lifetime N is rejected against N+1 and the version
  compare is only ever within one lifetime.
- **Language axis** is subsumed by incarnation: every reopen draws a fresh
  incarnation (so a relabel across lifetimes is already rejected), and within one
  lifetime the language does not change. The three-axis edit-path CAS
  (`incarnation && text && language`) therefore reduces to `incarnation && version`.

The incremental-parse **seed** re-homes onto `ParseScheduler`'s per-document state
(accumulated `InputEdit`s + the `base_version` they extend). Two obligations,
enforced by co-location today, become explicit scheduler invariants:

- The seed is applied to the snapshot's tree **iff
  `snapshot.parsed_version == base_version`**; on mismatch (a publish raced), it
  reseeds from the current snapshot and parses — never applies edits to a tree
  they do not match (the `#348` external-scanner corruption).
- A full-text sync **resets** `(pending_edits, base_version)`, so it parses from
  scratch. "Leaves no accumulated edits" is an explicit reset, not an emergent
  property.

### 3. Reader contract — non-blocking, three classes

Readers call a wait-free `latest_snapshot(uri)` (a `watch` borrow) and never parse
inline. A reader is at most one edit stale when `parsed_version < content_version`.

Region identity is the load-bearing subtlety. The `NodeTracker`
(lazy-node-identity-tracking) is edit-shifted **synchronously on `didChange`**, so
it tracks `content_version`; a stale-tree reader minting region-ids/ULIDs against
its own (`parsed_version`) byte ranges would write entries at positions the live
tracker has already moved — orphaned/duplicated ids, wrong-position reuse. Today
`compute_captures` avoids this only by *waiting* for the parse and re-snapshotting.
The decision therefore moves **region-id minting off every reader path**: the
parse-loop `populate` mints region ids once, at `parsed_version`, into the
snapshot's `injection_regions` (this is the injection-token-cache-reuse
"don't discover twice" lever, realized here as a consequence, not a separate
change). A stale-tree reader then only **reads** region ids from its own snapshot
— internally consistent with that snapshot's tree, minting nothing.

This **relocates** the tracker-skew hazard rather than fully closing it: `populate`
mints at `parsed_version` into a `NodeTracker` that has already been edit-shifted
to `content_version`, so a mispositioned entry can still land when the tracker is
ahead of the snapshot. Unlike the seed, the mint has no `parsed_version ==`-guard.
The residual is **performance-only**, not correctness: region-cache reads are gated
by a `validity_hash` (content + language), so a mispositioned id simply misses and
recomputes — it never serves wrong tokens — and it accumulates orphan entries that
lifecycle eviction reclaims. A future decision that forks a per-snapshot tracker
view would close it entirely; this decision accepts the perf residual.

Any reader that must resolve against **live** positions is position-critical
(below).

- **Serve-stale, actively refreshed** — `semanticTokens` full/delta. Compute
  against the snapshot's consistent `(text, tree, injection_regions)`; caches key
  off the snapshot's `(text, parsed_version)` so nothing is poisoned. When the
  parse loop publishes a fresh snapshot it emits `workspace/semanticTokens/refresh`.
  The trigger is added at the **parse-loop** publish point, **not** in the
  `didChange` handler — `didChange` deliberately does not emit refresh because a
  synchronous client (vim-lsp on Vim) cannot answer a server request while
  processing a notification; emitting from the off-ingress loop, after the
  notification returns, avoids that reentrancy.
- **Serve-stale, passively refreshed** — `documentSymbol`, `documentColor`.
  No server→client refresh exists for these, and adding one is out of scope; they
  serve the latest snapshot and self-correct on the client's **next** request
  (symbols/colors are re-requested on redraw/scroll, not held live). The staleness
  window is one edit and non-visual-jarring; this is the deliberate,
  user-sanctioned relaxation.
- **Staleness-reject** — `semanticTokens/range`, `kakehashi/node/*`,
  `selectionRange`, `formatting`, `rangeFormatting`, and `captures` (full and
  range). Their request positions/ranges are authored against the **live** text; a
  stale tree cannot answer them, and captures/node additionally resolve against the
  live `content_version` tracker. The rejection signal differs by protocol contract:
  - The LSP requests return **`ContentModified`** (-32801) when
    `content_version > parsed_version`. The spec does **not** mandate client
    auto-retry — a client that does not re-request simply gets the answer on its
    next natural request; formatting may no-op for that keystroke. Acceptable
    because these are not per-keystroke-critical, and it never serves a
    wrong-position result.
  - `kakehashi/captures` returns **`null`**, which is precisely the re-sync signal
    captures-protocol already defines ("on null, call full again") — a JSON-RPC
    *error* would violate that contract, since a captures client is only contracted
    to re-request on null. So a stale captures request serves `null` and the client
    re-issues, self-healing on its next request.

The only bounded wait that remains is the **first parse after `didOpen`** (the
snapshot is `None`); it waits briefly on `watch::changed()` then serves `null` —
the same decoupling parse-decoupled-document-lifecycle applies to its host tier.

*(Reclassifying `captures/full` and `kakehashi/node/*` off serve-stale is a
deliberate scope choice: making them serve-stale would require the snapshot to own
a versioned tracker view, a larger change than this decision commits to. If a
future decision forks the tracker per snapshot, they can move to serve-stale.)*

### 4. All tree-CPU runs on one bounded compute pool

A single dedicated `rayon` pool sized `max(2, num_cpus - 2)` — reserving at least
two cores for the tokio workers and the timer driver — runs **all** synchronous
tree work: the parse, `populate` (region minting + content hash), the
`compute_captures` / node layer walks, and the semantic-token injection fan-out
(folded off the process-global Rayon pool into this one). Async handlers hand a
whole work-unit to the pool (bridged by a `oneshot`) and `await` the result, so no
tree-CPU ever executes on a tokio worker. `DocumentParserPool`'s guard becomes a
sync mutex (`std`/`parking_lot`) since acquisition now runs on pool threads that
cannot `.await`.

The guarantee that document A cannot **starve** document B rests on two legs that
hold unconditionally: (1) no synchronous tree-CPU on the tokio workers, so B's
handler and the timer driver are always pollable; (2) the pool is bounded below
`num_cpus`, so A cannot oversubscribe the CPU and starve those workers. What
remains is bounded **queueing** on the compute pool itself: A's parse burst can sit
ahead of B's fan-out. `ParseScheduler` coalescing (already built) caps each
document to one queued parse, giving fairness across documents; a focused-document
priority — not yet built — would order *admission* so the document the user edits
drains first (rayon has no preemption, so this orders queueing, it cannot preempt
in-flight sub-tasks). The strong claim is therefore **no cross-document
starvation**; residual queue latency under many simultaneous large parses is
bounded by the coalescing + pool size, and the priority queue is a later knob.

### 5. Staged rollout

Delivered in independently shippable, reviewable, measurable stages that stay
inside the existing safety contracts at each step:

- **Stage 1 — tree-CPU off the async workers onto the bounded pool.** Move
  `populate` (all call sites) and the captures / node layer walks off the tokio
  workers; introduce the bounded pool and route parse + populate + walks + fan-out
  through it; convert `parser_pool` to a sync mutex. Delivers *no cross-document
  starvation* and most of *high-performance parsing*. The read path and the
  clear-tree-on-edit contract are untouched, so the stale-tree guarantee is not at
  risk; the sole new obligation is that `populate` is **awaited** (not detached),
  preserving the `populate → finish` ordering the injection-map invalidation
  depends on.
- **Stage 2 — versioned snapshot reads.** Introduce `SnapshotSlot` + the `watch`
  channel + `latest_snapshot`. **The parse loop dual-writes**: it keeps the legacy
  tree CAS (the incremental seed still reads `Document::tree`/`pending_seed`, which
  Stage 3 removes) **and** publishes a `ParseSnapshot` to the new channel, so
  `latest_snapshot` retains a servable tree across an edit's `tree.take()`. Move
  region minting into `populate`. Convert `semanticTokens` to serve-stale + the
  refresh trigger; `documentSymbol`/`documentColor` to serve-stale + passive;
  range/formatting/`node/*` to `ContentModified` and `captures` to `null` (its
  re-sync signal). Removes `get_tree_with_wait`
  / `wait_for_epoch` / `ensure_document_parsed` blocking. Delivers *instant reads*
  and *lifecycle independent of parsing*. Without the dual-write this stage would
  reproduce the empty-after-edit regression, so it is mandatory here, not deferred.
- **Stage 3 — consolidation.** Remove `Document::tree` / `pending_seed` (the seed
  now lives on the scheduler), collapse the CAS methods into the one publish
  primitive, delete the watermark waits, and prune the now-superseded passages
  from per-document-parse-scheduler (its tree-clear-on-edit and watermark/epoch
  sections) per delete-on-supersede — done **here**, when the behavior actually
  changes, not before.

## Considered Options

- **Keep the scheduler, only move `populate` to `spawn_blocking`.** Rejected as the
  endpoint: it removes the cross-document CPU starvation but not the per-keystroke
  read latency — with the tree still cleared on edit, readers keep blocking on the
  reparse (or fall into an on-demand full parse). And `spawn_blocking` **alone**
  trades HOL-blocking for CPU oversubscription on the default 512-thread blocking
  pool, so it must be paired with a bounded pool regardless. It is adopted as
  **Stage 1**, not the destination.
- **Serve the pre-edit / seeded tree immediately without versioning
  (naive stale-serve).** Rejected. It is crash-safe (the `#348` hazard is a
  parse-seed/external-scanner issue, not a query-cursor issue, and the read paths
  are already stale-offset-hardened), but readers **write** persistent caches. The
  whole-document token cache (in `semantic_cache` / `cache`, keyed by text hash) is
  closed by keying off the snapshot's text. The **injection-token cache** (keyed
  `(Url, region_id)`) and the **node-tracker ULIDs** are *not* closed by
  text-keying — they are closed only by moving region minting into the snapshot's
  `populate` (§3) so a stale read consumes stale-but-consistent ids and mints
  nothing. Naive stale-serve does neither and poisons both permanently (no
  generation bump, no refresh today).
- **A text-owning parse actor (per-document-parse-scheduler's Option 4).** Still
  rejected for the reason recorded there: reads bypass the owner, so owning the
  text buys nothing while resurrection-safety must still guard every reader
  fallback. This decision *reuses* the scheduler and its epoch rather than
  replacing them with an actor.
- **Block all readers until fully current (status quo).** Rejected: it is the
  behavior producing the 0.5–1.4 s waits. The user explicitly accepts a one-edit
  stale highlight (healed by refresh) in exchange for never blocking, with
  position-critical requests staleness-rejecting rather than serving stale.

## Consequences

### Positive

- Document lifecycle is fully decoupled from parsing: `didChange` never awaits a
  parse; readers never block on one.
- No cross-document starvation: with all tree-CPU on a bounded pool below
  `num_cpus`, a slow parse on one document cannot starve the async runtime or
  another document's requests.
- High-performance reads: highlight requests return against the latest snapshot
  immediately instead of waiting hundreds of ms for a reparse + populate.
- The scheduler ADR's residual **reader-fallback resurrection vector is
  eliminated**: wait-free borrowing readers never parse inline, so there is no
  reader path that can re-insert a document a `didClose` removed.
- State and coupling shrink: two `watch` maps collapse to one, six store CAS /
  watermark methods to one publish primitive, and the `pending_seed` invariant
  surface on `Document` is deleted (favorable on the State > Coupling > Complexity
  > Code ordering the repo optimizes for).

### Negative

- Highlight-class results may be **one edit stale for a few milliseconds** until
  the fresh snapshot publishes. `semanticTokens` is re-driven by an added
  post-edit `semanticTokens/refresh` (emitted from the parse loop to dodge the
  synchronous-client reentrancy that keeps it out of `didChange`);
  `documentSymbol`/`documentColor` self-correct only on the client's next natural
  request (no active refresh added).
- Staleness-reject requests (`kakehashi/node/*`, range, formatting) return
  `ContentModified` during the reparse window, and `kakehashi/captures` returns
  `null` (its protocol re-sync signal). The LSP spec does not mandate retry on
  `ContentModified`, so on a client that does not re-request (e.g. Neovim's built-in
  client for formatting) that keystroke's request **no-ops** until the next natural
  request; the captures `null` path is contracted to re-request. Reclassifying
  captures/node from the original serve-stale intent is the
  cost of not forking a per-snapshot tracker view now.
- The `#342`/`#374` stale-tree guarantee (a reader observes at least its own edit)
  is relaxed to "a reader observes a *consistent* but possibly one-edit-old
  snapshot, and knows it via the version tag." Correctness now rests on the version
  tag + refresh rather than on the reader having waited.
- Folding the semantic fan-out from the process-global Rayon pool (all cores) into
  the bounded `max(2, num_cpus - 2)` pool caps single-document tokenization
  throughput (≈ −50 % on a 4-core machine for one huge file) in exchange for
  cross-document isolation. Acceptable given the isolation goal, but a real
  single-doc regression to measure.
- Migration touches wide surface: `Document`, `DocumentStore`, `ParseScheduler`,
  the parse coordinator, `parser_pool` (Stage 1 sync-mutex conversion), and the
  ~17 `ensure_document_parsed` call sites. Each invariant from the scheduler and
  lifecycle decisions (edit_lock ordering, captures lineage, diagnostic republish
  order, incarnation CAS on reopen) must be re-proved against the snapshot model.

### Neutral

- `didClose` drops the single channel (waking any reader parked on the first-parse
  `watch::changed()`); a reopen replaces it with a fresh `SnapshotSlot`. A wait-free
  `latest_snapshot` borrow racing close+reopen may observe the prior lifetime's
  snapshot momentarily — bounded and self-healing via the next publish, the same
  class as today's borrow-then-reopen races.
- The `ParseScheduler` (one loop per document, coalescing, panic re-arm),
  `incarnation`, the `edit_lock`, and the incremental-seed concept are retained —
  re-homed, not removed.
- The bounded pool's size and the focused-document priority are a single knob;
  strict fairness (a per-document concurrency cap) can be added later without
  changing the model.

## Decision–Implementation Gap

Not yet implemented — this decision is aspirational and rolls out over the three
stages above. An interim fix advancing the parse watermark before `populate` (so
readers wake on the tree rather than after the populate) was prototyped and
measured as a partial improvement; it is subsumed by Stage 1 (which moves the
populate off the async workers) and Stage 2 (which removes the reader wait) and is
not carried forward on its own. The delete-on-supersede pruning of the overtaken
per-document-parse-scheduler passages is deferred to Stage 3, when the behavior
they describe is actually removed.
