# Parse Snapshot Architecture

**Related Decisions**: [per-document-parse-scheduler](per-document-parse-scheduler.md),
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md),
[injection-token-cache-reuse](injection-token-cache-reuse.md),
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
   `documentSymbol`, selection ranges — must **block** waiting for the reparse.
   The semantic-token reader (`get_tree_with_wait`) waits on the per-document
   parse **watermark** (`wait_for_epoch`), bounded to a 200ms budget; the
   captures / node readers wait via `ensure_document_parsed`.

2. **Cross-document blocking.** A slow reparse on document A stalls semantic
   tokens on an unrelated document B. Instrumentation showed the reader wait on
   a 300-line injection-heavy markdown file spiking to **0.5–1.4 s** per
   keystroke — far past the 200ms budget. The budget is defeated because the
   reparse's downstream CPU runs **inline on tokio worker threads**:
   `populate_injections` (the injection-cache build from injection-token-cache-reuse,
   O(regions): a per-region node-tracker ULID mint + content hash, hundreds of
   ms for ~900 regions) runs inline in the off-ingress parse loop, and
   `compute_captures` / the `kakehashi/node/*` layer walks run inline in their
   async handlers. On the default `num_cpus`-worker runtime those bursts saturate
   the worker pool, so the tokio timer wheel and `watch` notifications that back
   the 200ms caps cannot fire, and unrelated documents' handlers cannot be polled.

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
(`didOpen`/`didChange`/`didClose`) becomes fully independent of parsing: it
mutates inputs and returns, never awaiting or gating on a parse.

### 2. Parsing publishes a versioned `ParseSnapshot`

```
ParseSnapshot { text: Arc<str>, tree: Tree, language: String, incarnation: u64, parsed_version: u64 }
```

A snapshot's `text` is exactly the text its `tree` was parsed from — the two
always agree (the gopls immutable-snapshot property). Snapshots live per-URI in
a single `watch::Sender<Option<Arc<ParseSnapshot>>>`, which **subsumes both** the
current `parse_states` (`has_tree`) and `watermarks` (`ticket`) maps: `Some`
vs `None` is has-tree; `parsed_version` is the watermark; `incarnation` inside
the snapshot is the per-lifetime guard. The store's five tree/watermark CAS
methods collapse to **one publish primitive**: install a snapshot iff its
`incarnation` is current and its `parsed_version` is not older than the published
one. Monotonic version + incarnation give the same stale-drop / no-resurrection /
no-reopen-clobber guarantees the CAS methods encode today, in one place.

The incremental-parse **seed** re-homes onto `ParseScheduler`'s per-document
state (accumulated `InputEdit`s + the base version they extend), which already
*is* the per-document mutable parse state. A full-text sync leaves no accumulated
edits, so it parses from scratch — the `#348` incremental-contract stays closed
as a natural consequence rather than a special case.

### 3. Readers take a non-blocking latest snapshot

Readers call a wait-free `latest_snapshot(uri)` (a `watch` borrow) and never
parse inline. Two reader classes:

- **Highlight-class** (`semanticTokens` full/delta, `captures/full`,
  `kakehashi/node/*`, `documentSymbol`, `documentColor`): **serve-stale +
  self-heal.** Compute against the snapshot's internally-consistent
  `(text, tree)`. If `parsed_version < content_version` the result is at most one
  edit stale; the parse loop publishes a fresh snapshot moments later and emits
  `workspace/semanticTokens/refresh` (and the existing diagnostic republish) so
  the client re-requests. The client refresh plumbing already exists; only the
  post-edit trigger is added.
- **Position-critical** (`semanticTokens/range`, `captures/range`,
  selectionRange, formatting, rangeFormatting): the request's positions are
  authored against the **live** text, so a stale tree cannot be used. When
  `content_version > parsed_version`, return LSP **`ContentModified`** so the
  client retries — never map a live-text range onto stale bytes.

The only bounded wait that remains is the **first parse after `didOpen`** (the
snapshot is `None` and no prior snapshot exists); it waits briefly on
`watch::changed()` then serves `null`.

### 4. All tree-CPU runs on one bounded compute pool

A single dedicated `rayon` pool sized `max(2, num_cpus - 2)` — reserving at least
two cores for the tokio workers and the timer driver — runs **all** synchronous
tree work: the parse, `populate_injections`, the `compute_captures` / node layer
walks, and the semantic-token injection fan-out (folded off the process-global
Rayon pool into this one). Async handlers hand a whole work-unit to the pool
(bridged by a `oneshot`) and `await` the result, so no tree-CPU ever executes on
a tokio worker.

The guarantee that document A cannot block document B rests on **three legs
together**: (1) no synchronous tree-CPU on the tokio workers, so B's handler and
the timer driver are always pollable; (2) the pool is bounded below `num_cpus`,
so A cannot oversubscribe the CPU and starve those workers; (3) `ParseScheduler`
coalescing (already built) plus a focused-document priority queue keep A's jobs
from monopolizing the pool ahead of B and the document the user is editing.

### 5. Staged rollout

The change is delivered in independently shippable, reviewable, measurable
stages that stay inside the existing safety contracts at each step:

- **Stage 1 — tree-CPU off the async workers onto the bounded pool.** Move
  `populate_injections` (all call sites) and the captures / node layer walks off
  the tokio workers; introduce the bounded pool and route parse + populate +
  walks + fan-out through it. Delivers *no cross-document blocking* and most of
  *high-performance parsing*. The read path is untouched, so the stale-tree
  contract is not at risk.
- **Stage 2 — versioned snapshot reads.** Introduce `ParseSnapshot` + the `watch`
  channel + `latest_snapshot`; convert highlight-class readers to serve-stale +
  refresh and position-critical readers to `ContentModified`. Removes
  `get_tree_with_wait` / `wait_for_epoch` / `ensure_document_parsed` blocking.
  Delivers *instant reads* and *lifecycle independent of parsing*.
- **Stage 3 — consolidation.** Remove `Document::tree` / `pending_seed`, collapse
  the five store CAS methods into the one publish primitive, re-home the seed on
  the scheduler, delete the now-unused watermark waits.

## Considered Options

- **Keep the scheduler, only move `populate_injections` to `spawn_blocking`
  (the interim "fix D" + offload).** Rejected as the endpoint: it removes the
  cross-document CPU starvation but not the per-keystroke read latency — with the
  tree still cleared on edit, readers keep blocking on the reparse (or fall into
  an on-demand full parse). And `spawn_blocking` **alone** trades HOL-blocking for
  CPU oversubscription on the default 512-thread blocking pool, so it must be
  paired with a bounded pool regardless. It is adopted as **Stage 1**, not the
  destination.
- **Serve the pre-edit / seeded tree immediately without versioning
  (naive stale-serve).** Rejected. It is crash-safe (the `#348` hazard is a
  parse-seed/external-scanner issue, not a query-cursor issue, and the read paths
  are already stale-offset-hardened), but readers **write** persistent caches —
  the node-tracker ULIDs, the injection-token cache, and the whole-document token
  cache — keyed by the **new** text hash. Serving old-tree tokens under a
  new-text key poisons those caches permanently (no generation bump, no refresh
  today). The versioned snapshot fixes exactly this: caches key off the
  snapshot's own `(text, version)`, which are consistent, so nothing is poisoned.
- **A text-owning parse actor (the scheduler ADR's Option 4).** Still rejected
  for the reason recorded there: reads bypass the owner, so owning the text buys
  nothing while resurrection-safety must still guard every reader fallback. This
  decision *reuses* the scheduler and its epoch rather than replacing them with an
  actor.
- **Block all readers until fully current (status quo).** Rejected: it is the
  behavior producing the 0.5–1.4 s waits. The user explicitly accepts a one-edit
  stale highlight (healed by refresh) in exchange for never blocking, with
  position-critical requests staleness-rejecting rather than serving stale.

## Consequences

### Positive

- Document lifecycle is fully decoupled from parsing: `didChange` never awaits a
  parse; readers never block on one.
- No cross-document blocking: with all tree-CPU on a bounded pool below
  `num_cpus`, a slow parse on one document cannot starve the async runtime or
  another document's requests.
- High-performance reads: highlight requests return against the latest snapshot
  immediately instead of waiting hundreds of ms for a reparse + populate.
- State and coupling shrink: two `watch` maps collapse to one, five store CAS
  methods to one publish primitive, and the `pending_seed` invariant surface on
  `Document` is deleted (favorable on the State > Coupling > Complexity > Code
  ordering the repo optimizes for).

### Negative

- Highlight-class results may be **one edit stale for a few milliseconds** until
  the fresh snapshot publishes and a refresh re-drives the client. This is the
  deliberate, user-sanctioned tradeoff; it requires adding the post-edit
  `semanticTokens/refresh` trigger (absent today) and a captures-refresh path.
- Position-critical requests (range, formatting) now return `ContentModified`
  during the reparse window, relying on client retry — a behavior change from
  today's block-until-fresh.
- The `#342`/`#374` stale-tree guarantee (a reader observes at least its own
  edit) is relaxed to "a reader observes a *consistent* but possibly one-edit-old
  snapshot, and knows it via the version tag." Correctness now rests on the
  version tag + refresh rather than on the reader having waited.
- Migration touches wide surface: `Document`, `DocumentStore`, `ParseScheduler`,
  the parse coordinator, `parser_pool` (a `tokio::sync::Mutex` becomes a sync
  mutex once acquisition moves onto pool threads that cannot `.await`), and the
  ~13 `ensure_document_parsed` call sites. Each invariant from the scheduler and
  lifecycle decisions must be re-proved against the snapshot model.

### Neutral

- The `ParseScheduler` (one loop per document, coalescing, panic re-arm),
  `incarnation`, and the incremental-seed concept are retained — re-homed, not
  removed.
- The bounded pool's size and the focused-document priority are a single knob;
  strict fairness (a per-document concurrency cap) can be added later without
  changing the model.

## Decision–Implementation Gap

Not yet implemented — this decision is aspirational and rolls out over the three
stages above. An interim fix advancing the parse watermark before
`populate_injections` (so readers wake on the tree rather than after the populate)
was prototyped and measured as a partial improvement; it is subsumed by Stage 1
(which moves the populate off the async workers entirely) and Stage 2 (which
removes the reader wait) and is not carried forward on its own.
