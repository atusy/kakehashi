# Per-Document Parse Actor

## Context

The document-mutation path (`didOpen`, `didChange`, `didClose`) currently
spreads a single document's parse lifecycle across several handlers, a per-URI
lock, and an install coordinator that re-enters the parse path on completion.
Three forces collide here.

**Ingress ordering (#342, #374).** `IngressOrderGate`
(`src/lsp/ingress_order.rs`) assigns per-URI sequence tickets at `call` time so
that writers (`didOpen`/`didChange`/`didClose`) apply in strict wire order and
readers observe every edit that preceded them. `didOpen` was gated in #374 to
close the open→edit and close→reopen first-poll races. The gate holds a
document's writer ticket for the **whole handler future**.

**The slow, unbounded tail (#480).** Most of `did_open_impl`
(`src/lsp/lsp_impl/text_document/did_open.rs`) is fast or fire-and-forget, but
one branch is not: when the main language's parser is missing and auto-install
is enabled, the handler `await`s `maybe_auto_install_language` → `try_install`
**inside the writer-ticketed critical section**. Auto-install's network/git
stages are timeout-bounded, but parser *compilation* (`compile_parser` →
`tree-sitter-loader` `compile_parser_at_path`, `src/install/parser.rs`) shells
out to a C compiler with **no timeout**. A pathologically hung compiler holds
the `didOpen` writer ticket indefinitely, wedging every later same-URI reader
and writer. This is a *liveness* problem, not merely latency.

**Scattered parse lifecycle.** The current shape couples several concerns that
are hard to reason about independently:

- `did_open_impl` carries a `skip_parse` flag: when main-language auto-install
  fires, the handler skips `parse_document` because
  `reload_language_after_install` (`src/lsp/lsp_impl/coordinator/install.rs`)
  re-parses *after* the parser file is written. The install path therefore
  re-enters parsing from a different call site.
- `did_change_impl` serializes the non-atomic read-old-text → reparse → persist
  cycle with a per-URI `edit_lock` (`src/document/store.rs`), acquired as the
  handler's first `.await`. This is a practical mitigation layered on top of the
  ingress gate, not a structural guarantee.
- `reload_language_after_install` re-inserts and re-parses a document after a
  network/compile delay. If the document was closed in the meantime, that
  late re-parse can resurrect a closed document. #480 proposes to "reuse the
  existing eager-open supersede machinery" to cancel it — i.e. bolt a second
  lifecycle mechanism onto the parse path.
- Readers already tolerate insert-without-tree: they snapshot the shared
  `DocumentStore` and call `wait_for_parse_completion(200ms)` (a *bounded* wait
  with a fallback) before computing — see `semantic_tokens.rs`,
  `whole_document.rs`, `range_formatting.rs`.

The common root is that a document's text, parser readiness, and parse
scheduling have **no single owner**. Each handler mutates pieces of that state
under a shared lock, and the unbounded install runs on the latency-critical
ingress thread.

## Decision

Introduce a **per-document parse actor**: one actor task per open document that
exclusively owns that document's text, parser-readiness state, and parse
scheduling. The actor is the single consumer of a per-document mailbox, so
mutations to one document are serialized *by construction* rather than by a
shared lock.

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

The mailbox is **unbounded**, which is what makes the sends non-blocking. The
risk that normally argues against unbounded channels — memory growth under fast
input — does not apply here: `SetText` carries only a small delta, the actor
applies it to the owned text and drops it, and parse coalescing keeps at most one
parse in flight regardless of how many `SetText`s queue. So the queued state is
bounded by the document size, not by edit history. A bounded mailbox would
instead push backpressure onto the ingress thread, re-creating the stall the
design removes; it is therefore rejected.

### Install is shared and global; only the parse is the actor's child

Install is **not** a per-document operation the actor owns. `try_install`
dedupes per language: the second document of the same uninstalled language gets
an `AlreadyInstalling` outcome and skips its parse, while only the install
"winner" re-parses (via `reload_language_after_install`, which mutates **global**
state — pushing the data dir into `search_paths`, re-applying settings, and
calling `ensure_language_loaded` on the shared registry — *before* the
per-document `parse_document`). So install is already a shared, globally-mutating
operation today; what the actor design *adds* is that every waiting document
subscribes to its completion and parses on its own, rather than only the winner
re-parsing.

So the two operations are separated deliberately:

- **Global install** — shared, deduplicated, deadline-bounded (see below), owned
  by a process-wide installer. An actor in `Installing` *subscribes* to its
  completion; it does not own or abort it.
- **Per-document parse** — owned by the actor, the **only** step that writes the
  document's tree into the store, and the only child aborted on `Close`.

This split is what lets both the liveness fix and the resurrection guarantee
hold: aborting the per-document parse on `Close` never kills an install a
*sibling* actor still needs, and a global install completing after a `Close` is
harmless because the store-writing parse — the abortable per-actor child — is
already gone.

### State machine folds `skip_parse`

The actor holds an explicit state, which subsumes the `skip_parse` flag and the
install path's re-entry:

- `Uninstalled` → parser missing; the actor has subscribed to a shared install.
- `Installing` → same; `SetText` only applies the delta to the owned text (no
  parse).
- `Ready` → parser available; `SetText` schedules a parse.

On install completion the actor transitions `Installing → Ready` and parses the
**latest** text (not the open-time text). There is no separate
`reload_language_after_install` re-entry: re-parse is simply the actor's normal
`Ready` behaviour applied to current state.

### The actor owns the text; only the *parse* coalesces

LSP incremental `didChange` sends **deltas** (ranges + replacement), not full
text. `did_change_impl` reads `old_text` from the store and calls
`apply_content_changes_with_edits(&old_text, content_changes)` to derive both
the new text and the tree-sitter `InputEdits`. Deltas therefore **cannot be
coalesced by keeping only the latest** — dropping an intermediate delta
corrupts the text.

So the split is: the actor **applies every delta in order to the text it owns**
(a cheap string op, never dropped); only the **parse** coalesces. It keeps a
`latest_text` cell, a `dirty` flag, and the accumulated `InputEdits` since the
last completed parse:

```text
on SetText(delta):  latest = apply(latest, delta); pending_edits += delta.edits;
                    if parsing { dirty = true } else { start_parse(latest, take(pending_edits)) }
on parse_done:      if dirty { dirty = false; start_parse(latest, take(pending_edits)) }
```

If several `SetText`s land while a parse runs, every delta is applied to `latest`
in order, and the next parse runs once over the accumulated state — stale texts
never accumulate in the mailbox. A parse is incremental when a base tree exists,
feeding tree-sitter the **accumulated** `InputEdits` since the last parse; with
no base tree (e.g. just after install) it degrades cleanly to a full parse.
Incrementality is a performance optimization, never a correctness requirement.

This removes the `edit_lock` **from the parse / edit-application path**. The lock
exists today partly because the read-old-text → resolve-deltas → persist cycle
runs in concurrently-dispatched handlers against a shared store snapshot. Moving
delta application *into* the single-consumer actor — the actor, not the handler,
owns and mutates the text — is what makes that cycle non-interleavable. (A design
where the handler resolved deltas to text from a store snapshot *before* sending
would still race the snapshot and still need the lock.)

But the `edit_lock` carries a *second* duty the actor does **not** subsume:
`did_close_impl` holds it to serialize the captures-lineage clear
(`captures_cache.retain`) against a concurrent captures `full` reader's
`store_lineage`, which inserts under the same lock (captures-protocol). Because
readers bypass the actor, this reader-vs-close ordering is not covered by mailbox
serialization and needs a named replacement — either keep a dedicated lock for
the captures lineage, or fold the lineage write into the watermark contract.
This is a path the actor would otherwise silently orphan, so it is called out in
the Negative consequences.

### Reads bypass the mailbox

Read requests do **not** query the actor's mailbox. The actor *publishes* parse
results into the shared `DocumentStore`; readers snapshot the store directly,
exactly as today, using the existing `wait_for_parse_completion(200ms)` bounded
wait plus an empty/`null` fallback. Optionally, the actor publishes an
*install-pending* signal into the store (the store carries no such field today —
only `has_tree`/`in_progress`); a reader seeing it returns empty immediately
without waiting, since the parse cannot complete until an unbounded install
does. The existing `semantic_tokens_refresh` event — already emitted on language
load/reload — then prompts capable clients to re-request once the actor reaches
`Ready`. This saves only the 200ms bounded wait, so it is optional scope; the
load-bearing property is simply that the mailbox stays write-only, which keeps
the blocking problem from migrating to the read path.

### Reader-vs-writer ordering needs a published watermark

This is the subtle part, and it changes the writer-ticket timing, so it must be
designed explicitly rather than assumed.

Today `IngressOrderGate` completes a writer's ticket only *after* the whole
handler future resolves (`drop(gate)` follows `inner_fut.await` in
`ingress_order.rs`), and the `didChange` handler `await`s `parse_document`
*inside* that window. So a `semanticTokens` reader ticketed right after a
`didChange` is guaranteed to observe the **parsed** tree — locked down by
`gate_runs_reader_only_after_preceding_writer`. A reader's
`wait_for_parse_completion` then sees `has_tree == true` for the *current* tree.

Under the actor, the handler returns at **enqueue**, so the writer ticket
completes *before* the actor applies the delta or calls `mark_parse_started`. A
reader's gate barrier is then satisfied while `has_tree` is still true for the
**old** tree, and `wait_for_parse_completion` returns the stale tree
immediately. That reintroduces exactly the #342/#374 race. A bare `has_tree`
check is therefore insufficient.

The replacement: the actor publishes a per-URI **monotonic processed-through
watermark** keyed to the ingress writer sequence. Each mutation handler stamps
its enqueued message with the writer ticket it holds; when the actor finishes
applying-and-parsing the state through ticket *N* (and has written the resulting
tree into the store), it advances the published watermark to *N*. A reader
captures the current tail ticket at gate `call` time (it already does, for the
reader barrier) and waits — bounded by the existing 200ms — until the watermark
reaches that ticket, instead of waiting on `has_tree`. If an install is pending
the watermark cannot advance, so the reader simply times out into the empty
fallback, which is the intended install-pending behaviour. This preserves the
#374 reader-observes-preceding-writes guarantee without re-coupling the handler
to parse latency or to the unbounded install.

### Lifetime equals document lifetime

The actor's lifetime is exactly the open document's lifetime. `Close` aborts the
in-flight **parse** (the store-writing child), unsubscribes from the shared
install, and terminates the actor; the store entry is removed. The shared
install itself is *not* aborted — a sibling may still need it. Because the only
step that writes this document's tree into the store is the aborted per-actor
parse, a late install completing afterward has no actor to hand off to and
cannot re-insert the closed document — **resurrection is structurally
impossible**, with no separate supersede machinery. A reopen creates a fresh
actor.

### Complementary: install-wide deadline

Moving install off the ingress path stops it from wedging requests, but a hung
compiler should still not leak forever. An install-wide deadline (covering
`compile_parser`) bounds the shared installer itself, so a stuck compilation
eventually fails every subscribing actor instead of pinning a task forever. This
is complementary to the actor, not a substitute for it.

### Injection orchestration stays downstream

`process_injections` runs as a consequence of a completed main parse, not inside
the actor loop, so injection work never stalls main-document parsing. It is not
uniformly fire-and-forget, though: spawning external bridge servers is genuinely
fire-and-forget, but the `didChange` forwarding to already-opened virtual
documents is wire-order-sensitive and must be sequenced **after** the parse it
depends on (it needs the fresh tree) and in writer order — so it is ordered, not
loosed. The split between the fire-and-forget spawn and the ordered forwarding
must be preserved when re-homing this step.

## Considered Options

### 1. Minimal spawn of the slow tail (the literal #480 scope)

Keep the existing handlers and locks; only move auto-install (and optionally
`process_injections`) into a spawned task, and cancel that task on `didClose` by
reusing the eager-open supersede machinery.

Rejected as the end state. It fixes the immediate liveness exposure with a small
diff, but leaves `skip_parse`, the `edit_lock`, and the install re-entry in
place, and adds a *second* spawn/cancel lifecycle bolted onto the parse path —
more moving parts guarding the same invariant, not fewer. It remains a
reasonable interim step if the full actor is staged.

### 2. Install-wide deadline only

Add a timeout around the whole install (including compilation) so the writer
ticket is released after at most the deadline.

Rejected as insufficient. It bounds the worst case but still holds the ticket
for the deadline's duration, does nothing for ordinary latency, and leaves the
scattered lifecycle untouched. Retained as a *complementary* mitigation (see
Decision), not an alternative.

### 3. Per-document parse actor (chosen)

Serializes by construction, folds `skip_parse` into state, makes resurrection
impossible, and keeps install/parse off the ingress path — at the cost of an
ADR-sized refactor.

### 4. Actor answers read requests directly (mailbox-query reads)

Have read handlers send a request message to the actor and await its reply,
instead of reading a published store snapshot.

Rejected. It reintroduces the blocking it set out to remove: a read would queue
behind in-flight parses and buffered `SetText`s in the same mailbox. The
write-only-mailbox / shared-store-read split is load-bearing.

## Consequences

### Positive

- **Liveness.** An unbounded/hung install can no longer wedge same-URI readers
  or writers; the actor loop stays responsive and `Close` always lands.
- **Serialization by construction.** A single mailbox consumer removes the parse
  / edit-application path's reliance on the per-URI `edit_lock`; the
  read-old-text → reparse → persist cycle is no longer interleavable. (The
  lock's *other* duty — reader-vs-close captures-lineage ordering — is not
  subsumed; see Negative.)
- **`skip_parse` disappears**, folded into the `Uninstalled/Installing/Ready`
  state machine; the install path stops re-entering parsing from a second call
  site.
- **Resurrection is structurally impossible** — closing aborts the actor's
  store-writing parse, so a later shared-install completion has no actor to write
  through — without adding supersede machinery to the parse path.
- **One owner** for a document's text, its parse-readiness state, and parse
  scheduling (global install stays shared), which is easier to reason about than
  lock-guarded shared state.

### Negative

- **ADR-sized refactor** touching `did_open`, `did_change`, `did_close`, the
  install coordinator, the store, and every reader's wait path.
- **Lifecycle and cancellation must be exact.** Child-task abort on `Close`,
  completion plumbed back as messages, and mailbox backpressure are all new
  surface area to get right.
- **Carefully tuned races must be preserved**, not regressed: the captures
  lineage close ordering (captures-protocol) — which today rides the `edit_lock`
  the parse path sheds, so it needs an explicit replacement (a dedicated lock or
  the watermark contract); the geometry re-merge / no-op suppression on
  `didChange` (#422); and the diagnostic teardown ordering in `didClose`.
- **A new read-path coupling** is introduced: readers must wait on the actor's
  per-URI processed-through watermark instead of `has_tree`. Getting this watermark
  wrong silently reintroduces the #342/#374 stale-tree race.
- **Injection orchestration must be re-homed** carefully: the heavy external
  bridge-server spawn is fire-and-forget, but the bridge `didChange` forwarding
  to already-opened virtual documents is wire-order-sensitive and must stay
  ordered after the parse it depends on (or remain in the gated handler), not
  be loosed as unordered fire-and-forget.

### Neutral

- `IngressOrderGate` still issues per-URI tickets, but the writer ticket now
  completes at **enqueue** rather than after parse (see the watermark section),
  so its old reader-observes-preceding-writes guarantee is no longer carried by
  the ticket alone — it is carried by the actor's published watermark. Handlers
  **enqueue to the mailbox from within the gated critical section**, so mailbox
  FIFO still equals wire order and `Open`/`SetText`/`Close` reach the actor in
  the order #374 established. Non-parse side effects that must stay wire-ordered
  (diagnostic scheduling, the bridge `didChange` forwarding in
  `process_injections`) either remain in the gated handler or are sequenced by
  the actor *after* the parse they depend on; this re-homing is the
  highest-risk part of the change.
- Readers keep their existing bounded-wait + fallback contract, but the signal
  they wait on changes from `has_tree` to the per-URI processed-through
  watermark. That is the one read-path change the design requires; without it
  the new writer-ticket timing would let a reader observe a stale tree.

## Decision–Implementation Gap

Not yet implemented. This ADR records the target design; it runs ahead of the
code. As of writing, `did_open_impl` still awaits auto-install inline, still
carries the `skip_parse` flag, `did_change_impl` still serializes via
`edit_lock`, and `reload_language_after_install` still re-enters the parse path
on install completion, and readers still wait on `has_tree` (there is no
processed-through watermark, and no install-pending signal in the store). The
actor, its state machine, the child-task model, the reader watermark, and the
install-wide deadline are all unbuilt. A staged rollout may land Option 1
(minimal spawn) first as an interim liveness fix before the full actor.
