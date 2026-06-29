# Per-Document Parse Scheduler

**Related Decisions**:
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md),
[replace-tree-sitter-cli-with-loader](replace-tree-sitter-cli-with-loader.md),
[push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md),
[lazy-node-identity-tracking](lazy-node-identity-tracking.md)

## Context

This decision covers the **virt and native tiers** — the work that
intrinsically needs kakehashi's tree-sitter parse (region location for injected
languages, and the tree-derived `semanticTokens` / `selectionRange` /
`kakehashi/node` / `kakehashi/captures` features). The host tier needs no tree
and is decoupled separately; see parse-decoupled-document-lifecycle. With the host
tier hoisted ahead of the parse, what remains is to get the parse itself off the
latency-critical ingress path and give a document's parse lifecycle a single
owner.

Three forces motivate that.

**Liveness — the unbounded install tail (#480).** When the main parser is missing
and auto-install is enabled, `did_open_impl`
(`src/lsp/lsp_impl/text_document/did_open.rs`) `await`s
`maybe_auto_install_language` → `try_install` **inside the writer-ticketed
critical section**. Auto-install's network/git stages are timeout-bounded
(`run_with_timeout`, 120 s, killable), but parser *compilation*
(`compile_parser` → `tree-sitter-loader`'s `compile_parser_at_path`,
`src/install/parser.rs`) shells out to a C compiler **with no timeout and no
surfaced child process** — the `cc` subprocess lives inside the loader library
(see replace-tree-sitter-cli-with-loader). A hung compiler holds the `didOpen`
writer ticket indefinitely, wedging every later same-URI reader and writer.

**Cost — the parse is not cheap on injection-heavy documents.** It is tempting to
treat the parse as a few milliseconds, but for Markdown — where every inline span
is a `markdown_inline` injection and every fenced block is its own
injected-language parse — it is not. Measured on a release build against real
parsers, the document-ready latency (parse + injection, token computation
subtracted) is on the order of ~120 ms for a small document and rises to a few
hundred milliseconds for large ones; a single per-edit reparse on a 500-block
Markdown document costs roughly 50 ms, climbing with size. Crucially these are
*per-operation* costs, so a burst of edits costs real time per edit. **Coalescing
a burst into a single parse over the accumulated text is therefore a genuine
optimization, not a speculative one** — that is the force the parse owner must
answer.

**Change-path host immediacy requires the parse to be off-ingress.** Per
parse-decoupled-document-lifecycle, host-tier forwards must be immediate on the
*change* path too. If the parse runs inline in the change handler — holding the
writer ticket for the tens-to-hundreds of milliseconds above — the next
`didChange`, and any host forward sequenced behind it, waits on the parse. The
only way host-tier immediacy survives sustained editing is for the handler to
return before the parse runs: **parse off the ingress ticket.**

**Scattered parse lifecycle, and four names for one idea.** The current shape
couples concerns that are hard to reason about independently:

- `did_open_impl` carries a `skip_parse` flag: when main-language auto-install
  fires, the handler skips `parse_document` because
  `reload_language_after_install` (`src/lsp/lsp_impl/coordinator/install.rs`)
  re-parses *after* the parser file is written — a second entry point into the
  parse path.
- `did_change_impl` serializes the non-atomic read-old-text → reparse → persist
  cycle with a per-URI `edit_lock` (`src/document/store.rs`).
- `reload_language_after_install` can re-insert and re-parse a document after a
  network/compile delay; if the document was closed meanwhile, that late
  re-parse resurrects a closed document.
- Reader handlers carry an on-demand parse fallback
  (`try_parse_and_update_document` and analogues) that *writes the store*, and
  `update_document` **inserts on vacancy** — a second resurrection vector.

Underneath, the store already keeps **two** monotonic counters that are two
**axes** of one idea: `open_generations` (process-wide, used by the captures
lineage to detect close-then-reopen — an *incarnation* counter) and
`ParseState.generation` (per-lifetime, whose `mark_parse_finished` stale-check
already refuses to record a superseded parse). Neither gates the tree write
itself. The ingress writer ticket (`IngressOrderGate`,
`src/lsp/ingress_order.rs`) is a *third* per-document sequence — intra-lifetime
wire order, which restarts on reopen — and the reader-ordering watermark this
design needs would be a *fourth*. They are four implementations of two axes:
*which lifetime* (incarnation) and *where in that lifetime's wire order*
(ticket).

The common root is that a document's parser readiness and parse scheduling have
**no single owner**.

## Decision

Give a document's parse lifecycle a **single per-document owner that drives the
parse off the ingress ticket**, realized as a **per-document coalescing parse
scheduler**: the text stays in the shared document store, and the owner holds only
the parse-scheduling decision. Pair it with a **single per-document epoch** — the
pair `(incarnation, ticket)` — that is the one source of truth for ordering,
resurrection-safety, and parse-staleness: the retained process-wide open
generation (the incarnation, monotonic across reopen) paired with the ingress
writer ticket (intra-lifetime wire order), folding `ParseState.generation` and the
otherwise-new reader watermark into those two axes, and taking over the
`edit_lock`'s resurrection-guard duty at the tree write.

The owner is deliberately a scheduler rather than a mailbox actor that owns the
document's text (the actor is Option 4, considered and deferred below). Because
**reads bypass the owner either way** — readers snapshot the store, gated on the
epoch watermark — owning the text inside the owner would buy readers nothing; the
scheduler settles the same forces with less moving structure.

### Writes return at enqueue; the slow work runs off-ingress

`didOpen`, `didChange`, and `didClose` record their effect and return without
awaiting the parse. On `didChange` the edit is applied to the stored text, the
reader-visible tree is cleared, and the document is handed to the scheduler, which
runs the parse **off the ingress ticket**. The scheduler keeps **one parse in
flight per document** and **coalesces a burst**: an edit arriving while a parse
runs marks the document dirty rather than starting a second parse, so a run of
keystrokes collapses to a single reparse over the latest text — the coalescing the
cost force demands. Install is likewise off-ingress: the owner *subscribes* to a
shared installer's completion instead of awaiting it. So neither a slow parse nor
a hung compiler holds the ingress ticket, and a later `didClose` is never
sequenced behind them — the liveness fix.

A coalesced reparse can be **incremental** when a base tree exists (feeding
tree-sitter the accumulated edits since the last parse) and degrades cleanly to a
full parse otherwise. Incrementality is a performance optimization, never a
correctness requirement.

### One epoch for ordering, resurrection, and staleness

The four ad-hoc sequences are really **two axes** of one idea: an *incarnation*
(which open→close lifetime this is) and an *intra-lifetime ticket* (wire order
within one open document). The design makes the per-document **epoch** the pair
`(incarnation, ticket)`:

- the **incarnation** is the retained process-wide open generation
  (`open_counter` / `open_generations`), drawn fresh on every `didOpen`. It is
  deliberately **not** the ingress ticket: `DocumentSequencer::finish_close`
  (behind `IngressOrderGate`) removes a URI's sequencing state on close, so the
  ticket **restarts at 1 on reopen** and is not monotonic across lifetimes. Were the epoch the ticket
  alone, a straggler write from a previous lifetime could match the reopened
  document's ticket. The incarnation is the high half precisely to close that
  gap.
- the **ticket** is the ingress writer sequence within the current lifetime,
  plumbed to the handler from the gate.

Two derived quantities read off this one pair; the unification is of the
*source*, not of a single scalar:

- **Wire order / readiness (reader watermark).** Each mutation stamps its write
  with `(incarnation, ticket)`. The owner publishes a **processed-through
  watermark**: the highest ticket (within the current incarnation) whose state has
  reached a terminal outcome — `Parsed`, `NoTree` (parsed to nothing), or
  `InstallFailed`. It advances on **resolution**, not only on a successful tree
  write, or a parse/install failure would never advance it and readers would burn
  the full timeout. Because the owner **coalesces**, a single parse covers a
  contiguous run of tickets; the watermark advances to the highest ticket that
  parse **covered**, and a parse superseded by a newer edit before it completes
  does not strand its tickets — they are carried into the next (covering) parse.
  The watermark never regresses, but no bound is claimed on how quickly a *given*
  ticket is covered under sustained editing — continuous edits can keep the
  watermark behind a reader's target ticket indefinitely. The **hard** guarantee
  is the reader side: the existing 200 ms bound plus the empty fallback caps any
  wait, so sustained editing degrades a reader to the empty fallback rather than
  starving it. A virt/native reader waits — bounded by that 200 ms — until the
  watermark reaches the tail ticket that preceded it, then reads whatever the
  store holds (a tree, or empty), instead of waiting on `has_tree`. This is
  required because, with the handler returning at *enqueue*, a bare `has_tree`
  check would be satisfied while the store still holds the **old** tree —
  reintroducing the #342/#374 stale-tree race.
- **Resurrection-safety (epoch-checked CAS writes).** Every tree write becomes an
  **atomic, non-inserting** store update checked against the full
  `(incarnation, ticket)` pair: it no-ops if the document is gone (`Vacant`), the
  incarnation differs (a reopen happened), or the ticket moved. This folds both
  `open_generations` (incarnation mismatch) and `ParseState.generation` (ticket
  mismatch) into one comparison, generalizing the existing `mark_parse_finished`
  stale-check from the parse-state to the tree itself. The owner's parse is one
  writer; the reader on-demand fallbacks are the others (see below) — the guarantee
  holds only if **every** store-writing path uses this CAS.
- **Close-then-reopen detection** is the incarnation half: a reopen draws a fresh
  incarnation, so a stale lineage insert or a late parse from the previous
  lifetime fails the incarnation check even though the reopened lifetime's
  tickets restart at 1.

This requires new plumbing in `IngressOrderGate`: today the ticket is
middleware-private — a writer handler does not receive `gate.ticket()`, and a
reader handler does not receive the tail ticket the `ReaderBarrier` snapshots at
`call` time. Both must be exposed (via a task-local, a request extension, or by
moving the enqueue into the middleware). Crucially, because the epoch is the pair
`(incarnation, ticket)` and tickets restart on reopen, the **incarnation must be
plumbed alongside the ticket** — to both the writer (so its stamped write names
the right lifetime) and the reader (so the tail it waits on names
`(incarnation, ticket)`, not a bare ticket that a reopen's restarted sequence
could ambiguate). Equivalently, the reader can hold an incarnation-bound watermark
handle that is invalidated on close. The ingress middleware is part of the
refactor surface.

### Install is shared and global; only the parse is the owner's work

Install is **not** a per-document operation the owner owns. `try_install` dedupes
per language: the second document of the same uninstalled language gets an
`AlreadyInstalling` outcome, while the install "winner" mutates **global** state
(pushing the data dir into `search_paths`, re-applying settings, calling
`ensure_language_loaded`). So the two operations are separated deliberately:

- **Global install** — shared, deduplicated, deadline-bounded (see below), owned
  by a process-wide installer. The owner *subscribes* to its completion; it does
  not own or abort it. When the install completes, the owner parses the **latest**
  text (not the open-time text) — the completion-triggered reparse that
  `reload_language_after_install` performs today, now coordinated by the owner
  rather than re-entered from a second call site. (Collapsing the
  readiness/`skip_parse` structure into a single explicit state machine is unique
  to the Option-4 actor; the scheduler retains that structure.)
- **Per-document parse** — owned by the parse owner, the **authoritative**
  epoch-checked writer of the document's tree into the store, and the only work
  cancelled on close.

This split is what lets both the liveness fix and the resurrection guarantee
hold: cancelling the per-document parse on close never kills an install a
*sibling* document still needs, and a global install completing after a close is
harmless because the store-writing parse is already gone and its epoch is stale.

### Reads bypass the owner

Read requests do **not** route through the owner. The owner *publishes* parse
results into the shared `DocumentStore`; readers snapshot the store directly,
using the existing bounded wait — now keyed to the **epoch** rather than
`has_tree` — plus an empty/`null` fallback. Routing reads through the owner would
queue them behind in-flight parses and reintroduce exactly the blocking this
design removes; the publish-to-store / read-from-store split is load-bearing.

The reader on-demand parse fallbacks must stop being **unguarded store writers**:
they either return a **private** tree without writing the store, or persist
through the epoch-checked CAS write above. The present-and-text-unchanged guard
they use today is check-then-write over a store that inserts on vacancy, so a
concurrent `didClose` can slip between the check and the write and resurrect the
document; the CAS write closes that at the write itself.

Optionally, the owner publishes an *install-pending* signal into the store so a
virt/native reader returns empty immediately rather than burning the 200 ms wait,
with the existing `semantic_tokens_refresh` event prompting capable clients to
re-request once the parser is ready. This is optional scope; the load-bearing
property is that reads are served from the store, never through the owner.

### Lifetime equals document lifetime

The owner's lifetime is exactly the open document's lifetime. On `didClose` the
store entry is removed and the epoch advances; an in-flight parse is cancelled,
and any straggler write fails its epoch check (the incarnation no longer matches),
so **the install/parse resurrection path is structurally closed** with no separate
supersede machinery — *provided* every store-writing path, including the reader
on-demand fallback, goes through the epoch-checked CAS write above; that fallback
is the one residual vector, and it is closed at the write, not by a check a
concurrent close can slip past. A reopen draws a fresh incarnation. The shared
install is *not* aborted; a late completion has no document to hand off to and a
stale epoch.

Teardown must also wake any reader blocked on an epoch that will now never be
reached: dropping the epoch channel on close signals those waiters to proceed
(into the empty fallback), mirroring how the sequencer's
`DocumentSequencer::finish_close` drops its `done` sender. Without it a reader
racing the close would stall to the 200 ms timeout — bounded, but needless.

### Complementary: a real install deadline via a killable subprocess

Moving install off the ingress path stops it from wedging requests, but a hung
compiler should still not leak forever. A deadline here is **not** achievable by
wrapping the current future in `tokio::time::timeout`: compilation runs in
`spawn_blocking`, and `compile_parser_at_path` shells out to `cc` **without
surfacing the child** (see replace-tree-sitter-cli-with-loader), so a timeout
would let the `await` return while the blocking thread and its `cc` child keep
running. A real deadline requires running the loader's compile step inside a
**controllable subprocess / process group** that the installer can kill (and
reap) when the deadline fires, publishing failure only after termination. This
**reconciles with, and does not supersede,** replace-tree-sitter-cli-with-loader:
the loader still owns header/scanner/platform handling; we only wrap its call in
a killable boundary instead of reaching into the `cc` invocation. Complementary
to the parse owner, not a substitute.

### Injection orchestration stays downstream

`process_injections` runs as a consequence of a completed main parse, not on the
ingress path, so injection work never stalls main-document parsing. It is not
uniformly fire-and-forget, and it hides a **second instance of the liveness
hazard**: it awaits `check_injected_languages_auto_install` →
`maybe_auto_install_language(is_injection=true)` — the same untimed C compiler,
for an *injected* language. Re-homing it has three parts:

- **Injected-language auto-install** — must route through the *same* shared,
  deadline-bounded installer as the main language and be subscribed to, never
  awaited in a gated handler or on the parse path. A liveness requirement.
- **External bridge-server spawn** — genuinely fire-and-forget.
- **`didChange` forwarding to already-opened virtual documents** — wire-order
  sensitive; sequenced **after** the parse it depends on and in writer order, not
  turned into an unordered fire-and-forget task.

## Considered Options

### 1. Minimal spawn of the slow tail (the literal #480 scope)

Keep the existing handlers and locks; only move auto-install (and optionally
`process_injections`) into a spawned task, and cancel it on `didClose` by reusing
the eager-open supersede machinery.

Rejected as the end state. It fixes the immediate liveness exposure with a small
diff, but leaves `skip_parse`, the `edit_lock`, the install re-entry, and the
four ad-hoc sequences in place, and adds a *second* spawn/cancel lifecycle bolted
onto the parse path. It remains a reasonable interim step toward the chosen
off-ingress owner.

### 2. Install-wide deadline only

Add a timeout around the whole install so the writer ticket is released after at
most the deadline.

Rejected as insufficient. It bounds the worst case but still holds the ticket for
the deadline's duration, does nothing for ordinary parse latency, and leaves the
scattered lifecycle untouched. It is also harder than it looks — compilation runs
in non-cancellable `spawn_blocking` with no exposed child, so a real deadline
needs a killable subprocess, not a `timeout` wrapper. Retained as a
*complementary* mitigation, not an alternative.

### 3. A per-document coalescing scheduler, text left in the store (chosen)

Keep the text in the store and the concurrent handlers, but add a per-document
parse-scheduling owner: (a) every tree write is an epoch-checked, non-inserting
store update so resurrection is closed without owning the text, (b) install moves
off-ingress as a subscribe/notify, (c) the killable installer bounds compilation,
and (d) a per-document coalescing scheduler collapses a burst into a single
off-ingress reparse and keeps one parse in flight per document.

Originally rejected on the **cost** and **change-path** forces — the worry that a
decomposed design "has no natural place to coalesce a burst" and would "bolt on a
per-URI debounce that re-creates an ad-hoc owner." That objection did not hold:
the per-document scheduler **is** that owner, a small, self-contained coalescing
point rather than an ad-hoc one. It gives the same burst-coalescing the actor
would and keeps per-document parse concurrency, without a mailbox or a text-owning
task. The rejection's other prong — that decomposition spreads the epoch-check and
edit discipline across more call sites than a single consumer would — *does* hold,
and is accepted as a worthwhile tradeoff (see Negative and the Gap). Chosen because
it settles the same forces as Option 4 — the epoch (the largest simplification),
off-ingress parse and install, and resurrection-safety — with materially less
moving structure.

### 4. Per-document parse actor that owns the text (considered; deferred)

A mailbox actor per open document that **owns the text**. `didOpen` / `didChange`
/ `didClose` become non-blocking sends to an **unbounded mailbox**; the actor's
run loop `select!`s over the mailbox and the completion signals for the install
and the parse, never awaiting a long operation inline, so a `Close` is processed
even *during* a hung-compiler install. Because the actor is the **single
consumer** of edits, three properties follow that the scheduler does not have:

- it **applies every delta in order to the text it owns** and so **sheds the
  `edit_lock`** from the edit/parse path entirely — the read-old-text →
  resolve-deltas → persist cycle is non-interleavable by construction;
- it folds `skip_parse` into an explicit `Uninstalled/Installing/Ready` **state
  machine**, with no separate install re-entry; and
- `Close` aborts the in-flight parse child and **terminates the actor**.

It unifies the four sequences into one epoch, closes the resurrection path with
the same CAS writes, coalesces bursts, and keeps install/parse off the ingress
path — at the cost of an ADR-sized refactor, a new task-lifecycle and cancellation
surface, and an unbounded mailbox that cannot be capped without pushing
backpressure back onto the ingress thread (re-creating what the design removes).

Deferred in favor of Option 3. The actor's one distinguishing trait over the
scheduler is that it owns the document text, and that buys nothing for the decided
concerns: reads bypass the owner in both designs, so latency and throughput are
unchanged; the epoch, coalescing, and resurrection-safety are equally expressible
without text ownership; and the remaining epoch work an actor would absorb —
enforcing the incarnation axis on store writes — still has to guard the reader
fallbacks, which bypass the actor, so the actor would relocate that work rather
than remove it. Owning the text was expected to make incremental parsing cleaner;
in practice incremental parsing was achieved without it. The `edit_lock`-shedding
and `skip_parse` state-machine simplifications are real, but they are the *only*
gains unique to the actor and do not justify the extra structure. Retained as the
documented ideal — a no-behavior-change refactor worth revisiting only if a future
need turns out genuinely cleaner under single text ownership.

### 5. Actor answers read requests directly (mailbox-query reads)

A variant of Option 4 in which read handlers send a request message to the actor
and await its reply, rather than snapshotting the store.

Rejected. A read would queue behind in-flight parses and buffered edits in the
same mailbox, reintroducing the blocking the design removes. Serving reads from
the store rather than through the owner is load-bearing — and is why the chosen
scheduler keeps the text in the store in the first place.

## Consequences

### Positive

- **Liveness.** An unbounded/hung install can no longer wedge same-URI readers or
  writers: install is off-ingress and the handler returns at enqueue, so a
  `didClose` is never sequenced behind a hung compiler.
- **Burst coalescing.** A run of edits collapses to one parse over the
  accumulated text, saving the per-edit reparse cost on injection-heavy documents
  (the cost force).
- **One epoch, two axes, not four sequences.** Wire-order readiness,
  resurrection-safety, and parse-staleness key on a single composite epoch
  `(incarnation, ticket)` — the retained process-wide open generation paired with
  the ingress ticket — into which `ParseState.generation` and the reader watermark
  fold, and whose per-write check takes over the `edit_lock`'s resurrection-guard
  duty. The incarnation half keeps the epoch monotonic across a reopen even though
  tickets restart.
- **The install/parse resurrection path is structurally closed** — a close removes
  the store entry and advances the epoch, so a later parse or shared-install
  completion finds a stale epoch and its non-inserting write no-ops — without
  supersede machinery. This holds *provided* every store-writing path, including
  the reader on-demand fallback, goes through the epoch-checked CAS (see Negative).
- **One owner** for a document's parse-readiness and parse scheduling; the text
  stays in the shared store and the global install stays shared.

### Negative

- **Refactor surface** across `did_open`, `did_change`, `did_close`, the install
  coordinator, the store, the `IngressOrderGate` middleware (to plumb the ticket
  and tail ticket to handlers), and every virt/native reader's wait path.
- **An off-ingress parse lifecycle to manage**: scheduling, cancellation on close,
  and completion handling become explicit surface where previously the parse was
  inline.
- **Resurrection-safety is a multi-site proof obligation.** Because the scheduler
  keeps the concurrent handlers and the reader fallbacks bypass the owner, the
  epoch-checked write must be applied at *every* store-writing site rather than
  being a single-consumer invariant — the accepted cost of decomposing instead of
  funneling all writes through one actor.
- **Carefully tuned races must be preserved**: the captures-lineage close ordering
  still rides the retained `edit_lock` (the scheduler keeps it, so no replacement
  is needed); the geometry re-merge / no-op suppression on `didChange`
  (push-propagation-diagnostic-forwarding); and the diagnostic teardown ordering
  in `didClose`.
- **A new read-path coupling**: virt/native readers wait on the epoch instead of
  `has_tree`. Getting it wrong silently reintroduces the #342/#374 stale-tree
  race.
- **Reader on-demand parse fallbacks must stop being unguarded store writers**
  (`ensure_parsed_for_node_lookup` and analogues): return a private tree or
  persist through the epoch-checked CAS write, or the resurrection vector reopens.
- **Injection orchestration must be re-homed** carefully: injected-language
  auto-install through the shared deadline-bounded installer, bridge-server spawn
  fire-and-forget, and bridge `didChange` forwarding kept ordered after the parse.

### Neutral

- `IngressOrderGate` still issues per-URI tickets, but the writer ticket now
  completes at **enqueue** rather than after the parse; its old
  reader-observes-preceding-writes guarantee is carried by the epoch watermark
  instead. The writer stamps its effect from within the gated critical section, so
  the watermark order still equals wire order.
- Host-tier work is out of scope here — it neither needs the tree nor waits on the
  parse; see parse-decoupled-document-lifecycle.

## Decision–Implementation Gap

Realized as Option 3, the per-document coalescing scheduler. The parse runs off
the ingress ticket, a burst collapses to one reparse, install is off-ingress,
readers wait on the epoch watermark rather than on tree presence, store writes are
resurrection-safe (non-inserting), and injection orchestration is re-homed off the
ingress path.

Two architectural elements of the decision remain deferred. The epoch's
**incarnation** axis is not yet enforced on tree writes — only the intra-lifetime
ticket is — so a close-then-reopen race during an in-flight parse is not yet fully
closed. And the `didOpen` parse still runs on the ingress ticket rather than off
it (a one-time open cost; change-path immediacy is already covered). The
text-owning actor of Option 4 is not pursued; see Considered Options.
