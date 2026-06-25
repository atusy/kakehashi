# Capability Tier Parse Decoupling

**Related Decisions**: [per-document-parse-actor](per-document-parse-actor.md),
[host-document-bridge](host-document-bridge.md),
[push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md),
[ls-bridge-message-ordering](ls-bridge-message-ordering.md)

## Context

Opening a document should make it *usable* as fast as possible. But today the
`didOpen` and `didChange` handlers couple a document's host-language usability to
work it does not actually depend on: the tree-sitter parse and, worse,
auto-install.

**Most capabilities don't need our tree.** Kakehashi's features split into three
tiers by their dependence on kakehashi's own tree-sitter parse:

- **Host tier** — host-language `definition`/`hover`/`completion`/`references`/
  `rename`/`formatting`/`diagnostics`, and the act of *attaching* the host
  document to its external language server (the `_self` host bridge; see
  host-document-bridge). These forward to an external server keyed by the host
  language. They need the document **text and a language name only** — never the
  tree. `eager_open_host_document_on_servers`
  (`src/lsp/bridge/coordinator.rs`) takes `(settings, language: &str, uri,
  text)`; no tree, no loaded parser. The language name it needs is
  `language_for_path(path).or(language_id)`, a **path-based** resolution computed
  in `did_open_impl` *before* `ensure_language_loaded` — so it is available ahead
  of the parser-load attempt, not only ahead of the parse.
- **Virt tier** — the same features over *injected* regions. These need the tree
  **only to locate the region** under the cursor
  (`InjectionResolver::resolve_at_byte_offset(..., snapshot.tree(), ...)`); the
  forwarded request itself does not use the tree. Region location is an
  intrinsic tree dependency — you cannot know where the embedded Python is
  without parsing the Markdown.
- **Native tier** — `semanticTokens`, `selectionRange`, and the `kakehashi/node`
  / `kakehashi/captures` protocols. These are tree-derived and need a parsed
  tree fully.

**The coupling is incidental, not essential.** In `did_open_impl`
(`src/lsp/lsp_impl/text_document/did_open.rs`) the host attach is reached only
*after* three awaited steps that it does not depend on:

1. `maybe_auto_install_language` (when the main parser is missing and
   auto-install is on) — network/git plus an **unbounded** C-compiler step (see
   per-document-parse-actor for the liveness analysis);
2. `parse_document` plus the injection processing it feeds — measured together
   at **~120–310 ms** for a 50–2000-block injection-heavy Markdown
   (10 KB–435 KB), and bounded only by a 10 s parse timeout;
3. `process_injections` — which itself needs the tree.

The host attach is the *next* statement after those, even though
`eager_open_host_document_on_servers` consumes neither their results nor the
tree. So a user opening a 400 KB Markdown waits ~300 ms for host-language
diagnostics/hover that needed none of it; a user whose parser is auto-installing
(seconds, or a hung compiler forever) gets **no** host-language functionality
until the install resolves. That is pure ordering, not a data dependency.

On the edit path (`did_change_impl`) the analogous coupling is *text freshness*:
the post-delta text is persisted to the store by `parse_document`, so a host
forward that reads store text sees the new text only after the parse completes.

### Measured parse cost (release build, real parsers, injection-heavy Markdown)

| blocks | doc size | cold parse + injection | per-edit reparse + injection |
| -----: | -------: | ---------------------: | ---------------------------: |
|     50 |    10 KB |                ~120 ms |                            — |
|    500 |   107 KB |                ~146 ms |                       ~56 ms |
|   1000 |   215 KB |                ~186 ms |                       ~73 ms |
|   2000 |   435 KB |                ~308 ms |                      ~183 ms |

These are the latencies the host tier pays today for nothing (parse + injection
isolated from token computation; see per-document-parse-actor for method).

## Decision

**Classify every capability by tier, and gate each tier only on what it
intrinsically needs.** Concretely: hoist all host-tier work to the *front* of the
mutation handlers, ahead of parser load, parse, and install, so host-language
usability never waits on the tree.

### Host tier is hoisted ahead of parse and install

In `did_open_impl`, attach the host document to its external server(s)
**immediately after the document is registered** — before `ensure_language_loaded`,
before `parse_document`, before `maybe_auto_install_language`. The attach is
already fire-and-forget (it spawns per-server tasks); only its *position* moves.
Because it needs only the path-resolved language name (computed before the
parser-load attempt) and the text, nothing blocks it.

Consequence: a document whose parser is missing, installing, or hung still gets
full host-language-server functionality — attach, diagnostics, hover,
completion — at once. The unbounded install can delay only the **virt** and
**native** tiers, which is correct: those genuinely need the tree.

### Edit path keeps host text fresh without waiting for the parse

On `didChange`, persist the post-delta text to the store **before** scheduling
the parse, and let host forwards read that text. Applying a delta to the text is
a cheap string operation; the parse (incremental tree-sitter plus injection
reprocessing) is the expensive part the host tier must not wait on. The store's
existing per-URI `edit_lock` continues to serialize the read-old-text →
apply-delta → persist cycle against a concurrent `didClose` (the resurrection
guard), exactly as today.

### Ordering invariant: open-before-change survives hoisting

Hoisting must not let a host `didChange` overtake the host `didOpen` at the
external server. It cannot, for two independent reasons grounded in the current
bridge:

- **Single emitter, state-machine discriminated (the load-bearing reason).** All
  host forwards flow through `sync_host_document`
  (`src/lsp/bridge/text_document/host.rs`), whose per-`(uri, connection)` map entry
  decides the message: a *vacant* entry emits `didOpen`, an *occupied* entry emits
  `didChange`. A bare change before any open is structurally impossible — the first
  sync of a pair always finds the entry vacant and emits `didOpen`, *regardless* of
  which spawned task reaches the lock first. This carries the invariant on its own.
- **FIFO enqueue under the lock.** Within the locked region the message goes onto
  the per-connection FIFO via a non-blocking `try_send` with no `await` between
  lock and enqueue, and the single writer task drains in FIFO order — so the
  `didOpen`/`didChange` decided by the entry state cannot then be reordered on the
  wire. (Which task acquires the lock first is task-schedule order, not handler
  order; the entry-state discrimination is what makes that irrelevant.)

Hoisting the open *earlier* only strengthens open-before-change on the open path.
This invariant is stated explicitly here because it is what makes the
reordering safe; see ls-bridge-message-ordering for the connection-level
contract.

The invariant is about message **type** ordering (an open always precedes a
change for the same document), not text-**version** ordering. Whether a later
sync can carry *older* text than an earlier one — e.g. a delayed open-time task
emitting `didChange` with stale text after a newer sync already opened the
document — is a separate, **already-unresolved** concern: the content-version /
`content_epoch` stale-overwrite window (push-propagation-diagnostic-forwarding,
the #422 race). The `sync_host_document` fingerprint check only suppresses a
re-sync when the text is *unchanged*; it does not reject text that differs but is
older, so it does not close this race. Hoisting neither introduces nor fixes it;
this ADR's reordering is orthogonal to it.

### Virt and native tiers stay parse-gated

Virt-tier requests still wait on the parse, because region location needs the
tree; native-tier requests still wait, because they are the tree. Both keep their
existing bounded-wait-plus-fallback reader contract. While the parser is still
installing, these tiers return their empty fallback — the intended
"install-pending" behavior — while the host tier is fully live.

## Considered Options

### 1. Status quo — host attach after parse/install (rejected)

Leave the handler ordering as is. Rejected: it makes host-language UX hostage to
tree work it does not need, up to and including an unbounded compiler hold. The
measured 120–310 ms parse and the unbounded install are paid by every host-tier
feature for nothing.

### 2. Fold this into the parse-actor refactor only (rejected as the *first* step)

Wait for the full per-document parse actor (per-document-parse-actor) to land,
and let the actor's "non-parse side effects run first" structure deliver the
decoupling.

Rejected as the *sequencing*, not as the end state. The tier decoupling is a
small, self-contained reordering with the single largest UX payoff, and it does
not require the actor, the epoch unification, or the off-ingress parse. Tying it
to an ADR-sized refactor needlessly delays the win. It is recorded as its own
decision so it can ship first.

### 3. Tier decoupling, host hoisted to the front (chosen)

Reorder the handlers so host-tier work precedes everything it does not depend on,
with the open-before-change invariant made explicit. Independently shippable;
complementary to the parse actor, which later moves the virt/native parse itself
off the ingress path.

## Consequences

### Positive

- **Host-language UX is immediate** — attach, diagnostics, hover, completion are
  available as soon as the document is registered, regardless of parse latency or
  an installing/hung parser. This is the direct user-visible win.
- **The unbounded install is de-risked for the common case** even before the
  parse actor lands: a missing/installing main parser no longer blocks host-tier
  features (it still blocks virt/native, which the actor addresses separately).
- **A clear, documented contract** for which tier needs the tree, so future
  handlers are placed correctly by construction.

### Negative

- **A push-only host server may emit diagnostics against text whose parser never
  installs.** This is correct for the host tier (it is parser-independent), but
  it makes explicit an invariant the codebase must not violate: **nothing
  downstream may assume "host document opened ⟹ a parse exists."** Any code
  reached from the host attach must not read the tree.
- **Two code paths now run concurrently from one handler** (host-tier
  fire-and-forget vs. the parse and its dependents), so the handler's internal
  ordering — what must precede the parse vs. what may run after — has to be kept
  honest as handlers evolve.

### Neutral

- Virt and native tiers are unchanged in latency: they still wait for the parse,
  because they intrinsically depend on it. The decoupling does not make tree
  features faster; it stops non-tree features from waiting on them.
- The edit-path text-freshness change (persist text before parse) is a small
  structural reordering within `did_change_impl`; the per-document parse actor
  later generalizes it into full single-owner text application with parse
  coalescing.

## Decision–Implementation Gap

Not yet implemented. As of writing, `did_open_impl` attaches the host document
only after awaiting auto-install, `parse_document`, and `process_injections`, and
`did_change_impl` persists the post-delta text via `parse_document` (so host
forwards see new text only after the parse). The tier taxonomy is described here;
the handler reordering and the explicit open-before-change assertion are unbuilt.
This decision can land independently of, and before, the per-document parse actor.
