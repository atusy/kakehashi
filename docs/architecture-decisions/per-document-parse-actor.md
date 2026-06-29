# Per-Document Parse Actor

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
parsers:

| blocks | doc size | cold parse + injection | per-edit reparse + injection |
| -----: | -------: | ---------------------: | ---------------------------: |
|     50 |    10 KB |                ~120 ms |                            — |
|    500 |   107 KB |                ~146 ms |                       ~56 ms |
|   1000 |   215 KB |                ~186 ms |                       ~73 ms |
|   2000 |   435 KB |                ~308 ms |                      ~183 ms |

Method: each figure is the document-ready latency (didOpen→first response, or
didChange→response) with the warm-median token-compute time subtracted, isolating
parse + injection from token computation; driven synchronously one request at a
time, so these are per-operation costs, not a concurrent burst. A burst of edits
therefore costs real time per edit. Coalescing a burst into a single parse over
the accumulated text is a genuine optimization, not a speculative one.

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

The common root is that a document's text, parser readiness, and parse
scheduling have **no single owner**.

## Decision

Give a document's parse lifecycle a **single per-document owner that drives the
parse off the ingress ticket**, and a **single per-document epoch** — the pair
`(incarnation, ticket)` — that is the one source of truth for ordering,
resurrection-safety, and parse-staleness. The epoch pairs the retained
process-wide open generation (the incarnation, monotonic across reopen) with the
ingress writer ticket (intra-lifetime wire order), folds `ParseState.generation`
and the otherwise-new reader watermark into those two axes, and its per-write
check additionally takes over the `edit_lock`'s resurrection-guard duty.

The owner is a **per-document coalescing parse scheduler**: the text stays in the
shared document store, and the owner holds only the parse-scheduling decision —
collapsing a burst of edits into a single off-ingress reparse. It is deliberately
**not** a mailbox actor that owns the document's text (that is Option 4,
considered and deferred below). The two are architecturally equivalent for the
concerns this decision settles — off-ingress parsing, burst coalescing, the
epoch, and resurrection-safety — because **reads bypass the owner in either
design** (readers snapshot the store, gated on the epoch watermark), so owning the
text inside the owner buys readers nothing. The mechanism subsections below are
written in actor terms because that was the original formulation. Off-ingress
parsing, burst coalescing, the epoch, and resurrection-safety all carry over to
the chosen scheduler. Two properties do **not**, because they follow from owning
the text rather than from the off-ingress architecture: folding `skip_parse` into
an explicit state machine, and shedding the `edit_lock` by funneling edits through
a single consumer. Both belong to the deferred Option 4.

### Writes are messages; the loop never blocks on the slow op

`didOpen`, `didChange`, and `didClose` become non-blocking sends to the actor's
mailbox and return immediately:

- `didOpen` → `Open { text, language }`
- `didChange` → `SetText { content_changes }` (LSP deltas, applied in order)
- `didClose` → `Close`

The actor's run loop `select!`s over the mailbox **and** the completion signals
for the install it is waiting on and the parse it owns. It **never `.await`s a
long operation inline**:

```text
loop {
  select! {
    msg       = mailbox.recv()      => handle(msg),       // Open / SetText / Close
    installed = install_done.recv() => on_installed(),    // shared install completed
    parsed    = parse_done.recv()   => on_parsed(),       // this doc's parse completed
  }
}
```

Both the install and the parse run **off** the actor loop, their completion
arriving as just another message. Because the loop stays responsive to the
mailbox while either runs, a `Close` (and a queued `SetText`) is processed
*during* an in-flight install — even an unbounded, hung-compiler install. This
is the liveness fix: no ingress ticket and no mailbox is ever held hostage by
compilation.

The mailbox is **unbounded**, which is what makes the sends non-blocking. Deltas
are **never dropped** — dropping one corrupts the text, and standard LSP
incremental sync offers no server-initiated full-text resynchronization to
recover from a drop — so the mailbox cannot be capped by shedding messages. What
keeps memory in check is the actor's per-message cost: applying a delta to the
owned text is a cheap string op (parse and install are offloaded off the loop),
so the actor drains the mailbox far faster than an editor produces edits, and the
backlog stays transient even while a parse or an unbounded install is
outstanding. The actor further **drains all currently-ready `SetText`s and
applies them before scheduling a single parse**, collapsing a burst into one
parse rather than one per message — the coalescing the cost table makes
worthwhile. The deliberate tradeoff: the only lever that could hard-cap the queue
is transport backpressure, which would push the stall back onto the ingress
thread and re-create exactly what this design removes — so unboundedness is
accepted, relied on staying transient by the drain-rate argument, not by a drop
policy.

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

- **Wire order / readiness (reader watermark).** Each mutation handler stamps its
  enqueued message with `(incarnation, ticket)`. The actor publishes a
  **processed-through watermark**: the highest ticket (within the current
  incarnation) whose state has reached a terminal outcome — `Parsed`, `NoTree`
  (parsed to nothing), or `InstallFailed`. It advances on **resolution**, not
  only on a successful tree write, or a parse/install failure would never advance
  it and readers would burn the full timeout. Because the actor **coalesces**, a
  single parse covers a contiguous run of tickets; the watermark advances to the
  highest ticket that parse **covered**, and a parse superseded by a newer edit
  before it completes does not strand its tickets — they are carried into the
  next (covering) parse. The watermark never regresses and advances on a covering
  terminal completion, but no bound is claimed on how quickly a *given* ticket is
  covered under sustained editing — continuous edits can keep the watermark behind
  a reader's target ticket indefinitely. The **hard** guarantee is the reader
  side: the existing 200 ms bound plus the empty fallback caps any wait, so
  sustained editing degrades a reader to the empty fallback rather than starving
  it. A virt/native reader waits — bounded by that 200 ms — until the watermark
  reaches the tail ticket that preceded it, then reads whatever the store holds
  (a tree, or empty),
  instead of waiting on `has_tree`. This is required because, with the handler
  returning at *enqueue*, a bare `has_tree` check would be satisfied while the
  store still holds the **old** tree — reintroducing the #342/#374 stale-tree
  race.
- **Resurrection-safety (epoch-checked CAS writes).** Every tree write becomes an
  **atomic, non-inserting** store update checked against the full
  `(incarnation, ticket)` pair: it no-ops if the document is gone (`Vacant`), the
  incarnation differs (a reopen happened), or the ticket moved. This folds both
  `open_generations` (incarnation mismatch) and `ParseState.generation` (ticket
  mismatch) into one comparison, generalizing the existing `mark_parse_finished`
  stale-check from the parse-state to the tree itself. The actor's own parse is
  one writer; the reader on-demand fallback is the other (see below). The
  unguarded site today is `kakehashi/node`'s `ensure_parsed_for_node_lookup`
  (`entry.rs`); the `semanticTokens` and `selectionRange` fallbacks are already
  `edit_lock`-guarded but switch to the same CAS write.
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
plumbed alongside the ticket** — to both the writer (so its stamped message names
the right lifetime) and the reader (so the tail it waits on names
`(incarnation, ticket)`, not a bare ticket that a reopen's restarted sequence
could ambiguate). Equivalently, the reader can hold an incarnation-bound watermark
handle that is invalidated on close. The ingress middleware is part of the
refactor surface.

### Install is shared and global; only the parse is the actor's child

Install is **not** a per-document operation the actor owns. `try_install` dedupes
per language: the second document of the same uninstalled language gets an
`AlreadyInstalling` outcome, while the install "winner" mutates **global** state
(pushing the data dir into `search_paths`, re-applying settings, calling
`ensure_language_loaded`). So the two operations are separated deliberately:

- **Global install** — shared, deduplicated, deadline-bounded (see below), owned
  by a process-wide installer. An actor in `Installing` *subscribes* to its
  completion; it does not own or abort it.
- **Per-document parse** — owned by the actor, the **authoritative** epoch-checked
  writer of the document's tree into the store, and the only child aborted on
  `Close`.

This split is what lets both the liveness fix and the resurrection guarantee
hold: aborting the per-document parse on `Close` never kills an install a
*sibling* actor still needs, and a global install completing after a `Close` is
harmless because the store-writing parse is already gone and its epoch is stale.

### State machine folds `skip_parse`

The actor holds an explicit state, which subsumes the `skip_parse` flag and the
install path's re-entry:

- `Uninstalled` → parser missing; the actor has subscribed to a shared install.
- `Installing` → same; `SetText` only applies the delta to the owned text (no
  parse).
- `Ready` → parser available; `SetText` schedules a parse.

On install completion the actor transitions `Installing → Ready` and parses the
**latest** text (not the open-time text). There is no separate
`reload_language_after_install` re-entry: re-parse is the actor's normal `Ready`
behavior applied to current state.

### The actor owns the text; only the *parse* coalesces

LSP incremental `didChange` sends **deltas**, not full text, so deltas **cannot
be coalesced by keeping only the latest** — dropping an intermediate delta
corrupts the text. The split is: the actor **applies every delta in order to the
text it owns** (a cheap string op, never dropped); only the **parse** coalesces.
It keeps a `latest_text` cell, a `dirty` flag, and the accumulated `InputEdits`
since the last completed parse:

```text
on SetText(delta):  latest = apply(latest, delta); pending_edits += delta.edits;
                    if parsing { dirty = true } else { parsing = true; start_parse(latest, take(pending_edits)) }
on parse_done:      if dirty { dirty = false; start_parse(latest, take(pending_edits)) } else { parsing = false }
```

A parse is incremental when a base tree exists, feeding tree-sitter the
**accumulated** `InputEdits` since the last parse; with no base tree (e.g. just
after install) it degrades cleanly to a full parse. Incrementality is a
performance optimization, never a correctness requirement.

This removes the `edit_lock` **from the parse / edit-application path**: moving
delta application into the single-consumer actor makes the read-old-text →
resolve-deltas → persist cycle non-interleavable by construction. But the
`edit_lock` carries a *second* duty the actor does **not** subsume:
`did_close_impl` holds it to serialize the captures-lineage clear
(`captures_cache.retain`) against a concurrent captures `full` reader's
`store_lineage` insert. Because readers bypass the actor, this reader-vs-close
ordering needs a named replacement — either a dedicated lock for the captures
lineage, or folding the lineage write into the epoch check (a stale lineage
insert fails its epoch check, the same mechanism that guards tree writes).

### Reads bypass the mailbox

Read requests do **not** query the actor's mailbox. The actor *publishes* parse
results into the shared `DocumentStore`; readers snapshot the store directly,
using the existing bounded wait — now keyed to the **epoch** rather than
`has_tree` — plus an empty/`null` fallback. Having reads queue behind in-flight
parses and buffered `SetText`s in the same mailbox would reintroduce exactly the
blocking this design removes; the write-only-mailbox / shared-store-read split is
load-bearing.

The reader on-demand parse fallbacks must stop being **unguarded store writers**:
they either return a **private** tree without writing the store, or persist
through the epoch-checked CAS write above. The present-and-text-unchanged guard
they use today is check-then-write over a store that inserts on vacancy, so a
concurrent `didClose` can slip between the check and the write and resurrect the
document; the CAS write closes that at the write itself.

Optionally, the actor publishes an *install-pending* signal into the store so a
virt/native reader returns empty immediately rather than burning the 200 ms wait,
with the existing `semantic_tokens_refresh` event prompting capable clients to
re-request once the actor reaches `Ready`. This is optional scope; the
load-bearing property is that the mailbox stays write-only.

### Lifetime equals document lifetime

The actor's lifetime is exactly the open document's lifetime. `Close` aborts the
in-flight **parse** (the store-writing child), unsubscribes from the shared
install, advances the epoch, and terminates the actor; the store entry is
removed. The shared install itself is *not* aborted. A late install completing
afterward has no actor to hand off to, and any straggler write fails its epoch
check (the incarnation no longer matches), so **the install/parse resurrection
path is structurally closed** with no separate supersede machinery — *provided*
every store-writing path, including the reader on-demand fallback, goes through the
epoch-checked CAS write above; that fallback is the one residual vector, and it is
closed at the write, not by a check a concurrent close can slip past. A reopen
creates a fresh actor at a fresh incarnation.

Teardown must also wake any reader blocked on an epoch that will now never be
reached: dropping the epoch channel on actor termination signals those waiters to
proceed (into the empty fallback), mirroring how the sequencer's
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
to the actor, not a substitute.

### Injection orchestration stays downstream

`process_injections` runs as a consequence of a completed main parse, not inside
the actor loop, so injection work never stalls main-document parsing. It is not
uniformly fire-and-forget, and it hides a **second instance of the liveness
hazard**: it awaits `check_injected_languages_auto_install` →
`maybe_auto_install_language(is_injection=true)` — the same untimed C compiler,
for an *injected* language. Re-homing it has three parts:

- **Injected-language auto-install** — must route through the *same* shared,
  deadline-bounded installer as the main language and be subscribed to, never
  awaited in a gated handler or the actor loop. A liveness requirement.
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
the per-document scheduler **is** that owner, and it is a small, self-contained
coalescing point rather than an ad-hoc one. It gives the same burst-coalescing the
actor would and keeps per-document parse concurrency, without a mailbox or a
text-owning task. The rejection's other prong — that decomposition spreads the
epoch-check and edit discipline across more call sites than a single consumer
would — does hold, and is accepted as a worthwhile tradeoff (see Negative and the
Gap). Chosen because it settles the same forces as Option 4 — the epoch (the
largest simplification), off-ingress parse and install, and resurrection-safety —
with materially less moving structure.

### 4. Per-document parse actor that owns the text (considered; deferred)

A mailbox actor per open document that **owns the text**, serializes by
construction, folds `skip_parse` into an explicit state, unifies the four
sequences into one epoch, closes the resurrection path with CAS writes, coalesces
bursts, and keeps install/parse off the ingress path — at the cost of an
ADR-sized refactor and a new task-lifecycle and cancellation surface.

Deferred in favor of Option 3. The actor's one distinguishing trait over the
scheduler is that it owns the document text, and that buys nothing for the decided
concerns: reads bypass the owner in both designs, so latency and throughput are
unchanged; the epoch, coalescing, and resurrection-safety are equally expressible
without text ownership; and the remaining epoch work an actor would absorb —
enforcing the incarnation axis on store writes — still has to guard the reader
fallbacks, which bypass the actor, so the actor would relocate that work rather
than remove it. Owning the text was expected to make incremental parsing cleaner;
in practice incremental parsing was achieved without it. Retained as the
documented ideal — a no-behavior-change refactor worth revisiting only if a future
need turns out genuinely cleaner under single text ownership.

### 5. Actor answers read requests directly (mailbox-query reads)

Have read handlers send a request message to the actor and await its reply.

Rejected. A read would queue behind in-flight parses and buffered `SetText`s in
the same mailbox, reintroducing the blocking the design removes. The
write-only-mailbox / shared-store-read split is load-bearing.

## Consequences

These are stated for the actor formulation. All hold for the chosen scheduler
except the two that specifically concern owning the text — a single owner of the
document *text*, and folding `skip_parse` into the actor's state machine — which
pertain to the deferred Option 4.

### Positive

- **Liveness.** An unbounded/hung install can no longer wedge same-URI readers or
  writers; the actor loop stays responsive and `Close` always lands.
- **Burst coalescing.** A run of edits collapses to one parse over the
  accumulated text, saving the measured per-edit reparse cost on injection-heavy
  documents.
- **One epoch, two axes, not four sequences.** Wire-order readiness,
  resurrection-safety, and parse-staleness key on a single composite epoch
  `(incarnation, ticket)` — the retained process-wide open generation paired with
  the ingress ticket — into which `ParseState.generation` and the reader watermark
  fold, and whose per-write check additionally takes over the `edit_lock`'s
  resurrection-guard duty. The incarnation half keeps the epoch monotonic across a
  reopen even though tickets restart.
- **`skip_parse` disappears**, folded into the `Uninstalled/Installing/Ready`
  state machine; the install path stops re-entering parsing from a second call
  site.
- **The install/parse resurrection path is structurally closed** — `Close` aborts
  the actor's store-writing parse and advances the epoch, so a later
  shared-install completion has no actor and a stale epoch — without adding
  supersede machinery. This holds *provided* the reader on-demand fallback also
  writes through the epoch-checked CAS path (the one residual vector; see
  Negative), not as an unconditional property of `Close` alone.
- **One owner** for a document's text, parse-readiness, and parse scheduling
  (global install stays shared).

### Negative

- **ADR-sized refactor** touching `did_open`, `did_change`, `did_close`, the
  install coordinator, the store, the `IngressOrderGate` middleware (to plumb the
  ticket and tail ticket to handlers), and every virt/native reader's wait path.
- **Lifecycle and cancellation must be exact.** Child-task abort on `Close`,
  completion plumbed back as messages, and the unbounded mailbox are new surface
  area.
- **Carefully tuned races must be preserved**: the captures-lineage close
  ordering (it rides the `edit_lock` the parse path sheds, so it needs the
  dedicated-lock-or-epoch-check replacement); the geometry re-merge / no-op
  suppression on `didChange` (push-propagation-diagnostic-forwarding); and the
  diagnostic teardown ordering in `didClose`.
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
  completes at **enqueue** rather than after parse; its old
  reader-observes-preceding-writes guarantee is carried by the published epoch
  instead. Handlers **enqueue from within the gated critical section**, so mailbox
  FIFO still equals wire order.
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
