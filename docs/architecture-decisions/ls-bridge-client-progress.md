# LS Bridge Client Progress

**Related Decisions**: [ls-bridge-work-done-progress](ls-bridge-work-done-progress.md), [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md), [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md), [ls-bridge-progress-disconnect-cleanup](ls-bridge-progress-disconnect-cleanup.md)

## Context

A client (editor) request may carry its own progress tokens: a `workDoneToken`
(a UI progress indicator) and a `partialResultToken` (streamed result chunks).
These differ from the server-declared tokens handled in
ls-bridge-work-done-progress: client-provided tokens are already globally unique
because the client mints them, so token *namespacing* — the problem that
motivated the `ProgressRegistry` — does not apply here.

The problem is **fan-out**. The bridge multiplexes one editor request onto
several downstream servers (e.g. a references request spanning several injected
languages, fanned out per language-server-bridge-request-strategies). Each
downstream may emit work-done progress (`$/progress` against the `workDoneToken`)
and partial results (against the `partialResultToken`). Forwarding the work-done
stream verbatim duplicates the lifecycle (N `Begin`, N `End`) and corrupts the
indicator; concatenating result chunks verbatim mis-orders data. So, under
fan-out, **the bridge must own the upstream terminal**: it cannot blindly relay
all N downstreams' `End`s/responses (they collide); it forwards exactly one or
composes one, as the Decision details. (A request that reaches a single server is
just relayed.)

Originally the bridge forwarded no client-provided tokens to downstreams at all.
The host raw-request path strips them (`strip_progress_tokens`,
`src/lsp/bridge/text_document/host.rs`), and the per-method virtual request
builders constructed fresh params with default (empty) progress fields, so no
token was carried either way. (The host path still strips, and
`partialResultToken` is still dropped; wired methods now carry a bridge-minted
`workDoneToken` — see the Decision–Implementation Gap.) With the token stripped
the downstream had none to report against, and the bridge discarded downstream
notifications regardless — so client-requested progress did not reach the editor
(and a server that did emit `$/progress` could legally still return an empty final
result). This is what the decision below set out to fix; it now reaches the editor
on the wired paths.

## Decision

Stop stripping client-provided tokens (selectively), route the resulting
downstream→upstream `$/progress` and partial-result notifications, and aggregate
them so the editor sees **one coherent lifecycle** that stays consistent with the
result actually delivered.

**Core principle.** Whatever the editor sees stays consistent with the result
actually delivered: the work-done `End` coincides with the result being complete,
and no `partialResultToken` chunk is streamed that the delivered result will not
contain. How the title-bearing `Begin` is produced then depends on the strategy,
because the two deliver differently:

- *preferred* delivers a single **winner**, and progress is **anchored** on the
  **anchor** — the highest-priority *named* candidate — whose own `Begin` is the
  title; only the anchor's own progress is tracked. The winner (the delivered
  source) is usually that anchor, but the priority walk resolves the wildcard
  `Rest` group — the `"*"` element of `priorities` (aggregation-priorities-wildcard),
  i.e. every server not named explicitly — first-win by earliest non-empty
  arrival, so a `Rest` member can be
  the winner without being an anchor: its own progress is **not** shown (no safe
  a priori title for a latency race), unless it is the sole server (N = 1). A
  candidate that returns empty, or fails before producing any data, is no winner
  (reserved for the delivered source, possibly a promoted partial prefix), so the
  anchor falls through to the next *named* candidate. The bridge never forwards a
  non-anchor's `Begin` as the title. (One documented edge: after a fall-through,
  an earlier anchor's title can linger; see Consequences.)
- *concatenated* merges **all** contributors, so no single source owns the title:
  when progress is shown at all (see the engages-only-when-staggered rule below)
  the bridge **composes its own synthetic neutral `Begin`** (never borrowing a
  downstream's title; e.g. a request-derived one) and reports collection
  progress, **ignoring every downstream's own `$/progress`**. This trades
  per-server granularity for a title that stays accurate across the whole
  aggregate — borrowing one contributor's title would go stale the moment that
  contributor finishes while others are
  still pending (see Consequences).

The lifecycle engages only when there is progress worth showing; otherwise the
request just returns its result (today's behavior, minus the strip).

- **Single downstream (N = 1).** Relay that server's own `Begin`/`report`/`End`
  onto the client's original token — what the editor sees. (Client tokens are
  already unique, so the server-declared `ProgressRegistry` namespacing is not
  involved; internally the bridge mints a per-server token so the reader can route
  the downstream's `$/progress` to the request's aggregator, which retargets it
  onto the client token.) If it emits none, the editor sees no progress. This
  holds even when the sole server was selected via the wildcard: the
  `Rest`-never-anchor rule disambiguates among *racing* contenders, and a single
  downstream has none — so its real `Begin` is safe to forward.
- **preferred, N > 1.** This strategy short-circuits (it does not wait for the
  losers), so there is no collection count to report. If the anchor emits its own
  progress, forward its `Begin`/`report`/`End` (real title) and suppress every
  other candidate's progress; if the anchor returns a complete result with no
  progress phase, show nothing.
- **concatenated, N > 1.** The bridge waits for every contributor and
  **drives the whole lifecycle itself**: synthesize a neutral `Begin` once
  collection is known
  to be staggered (a result is in while others are still pending), report `n/m` as
  each result arrives, and fire one `End` when **every** contributor is collected.
  Downstreams' own `Begin`/`report`/`End` are **ignored entirely** — there is no
  anchor `Begin` to forward, suppress, or wait for. If all results are already in
  at first observation, no progress is shown. Accumulated `report`s may be
  coalesced into one notification (`Begin → report → report (2, 3) → End`) to
  bound notification volume.
- **Begin/End pairing invariant.** An open work-done `Begin` is closed by exactly
  one `End`, fired when the *request* terminates — normal completion, anchor
  failure, fall-through exhaustion, or client cancellation (the fan-in returns
  `Cancelled`). Under *preferred* (and N = 1) there is one open lifecycle per
  token, normally closed by the tracked source's real `End`; after a fall-through
  the open `Begin` may be an earlier anchor's while the new anchor supplies the
  `End`, and the bridge synthesizes the `End` if the source died, was cancelled,
  or completes with no progress phase. Under *concatenated* the
  bridge composes the whole lifecycle, so the terminal is always its own
  aggregate-timed `End` (fired when all results are collected, or synthesized on
  cancellation). A request that opened no `Begin` emits no `End`. (The per-branch
  rules above are instances of this invariant.)
- **Graceful degradation on committed-server failure.** The branch turns on
  *whether the editor has already been shown data*:
  - *preferred, winner already streamed partials*: data is on screen, so promote
    the winner's accumulated partials into the delivered result and complete the
    request. Do not wait for another candidate (that would freeze the shown data)
    and do not swap in a different server's result. The result may be incomplete;
    accepted as graceful degradation. (Progress follows the pairing invariant:
    `partialResultToken` data and `workDoneToken` progress are separate tokens, so
    any open work-done `Begin` is closed by a synthetic `End`; a winner that
    streamed only partial-result data opened none and needs no `End`.)
  - *preferred, candidate produced nothing* (empty or failed before any partial
    result): it falls through to the next-priority candidate — down to a `Rest`
    winner, or to no result at all — exactly as any empty result does under
    preferred. Nothing was committed, so this is ordinary latency, not a freeze or
    swap. Progress follows the pairing invariant: if a prior *named* anchor opened
    a `Begin`, it stays open (its title can linger — see Consequences) and is
    closed by the next named anchor's real `End`, or by a synthetic `End` at
    request completion (when the next winner is a `Rest` member or emits no
    progress, or no result is returned at all); if no `Begin` was opened, none is
    emitted.
  - *concatenated*: a failed contributor contributes whatever it already streamed
    (possibly nothing) and is **dropped from the expected set** (the `n/m`
    denominator shrinks); the others proceed, and the aggregate `End` fires once
    the remaining expected results are collected. Because the bridge owns the
    synthetic title and terminal here, a failed contributor never affects the
    `Begin` — there is no anchor handoff.
- **partialResultToken — translate, then merge.** Partial-result chunks carry
  locations needing the *same* injection offset and URI translation as final
  responses, applied incrementally per chunk through the existing aggregation
  path (which assumes a single final blob today and must accept incremental
  input). Under *preferred* the bridge streams the winner's translated chunks.
  Under *concatenated* the final result is priority-ordered, so chunks must be
  released in that same order (buffer per contributor, flush in priority order) —
  otherwise the streamed prefix would diverge from the delivered ordering. This
  costs some streaming latency; see the gap.

The terminal `End` the bridge emits on failure is the same primitive
ls-bridge-progress-disconnect-cleanup uses for server-declared tokens — the
bridge composes the terminal rather than relaying a downstream's `End`.

## Considered Options

- **Keep stripping both tokens (status quo).** Simplest and zero-risk, but
  client-requested progress never surfaces — the gap this decision closes.
  Rejected.
- **Latency-based selector (track the first responder).** Lower time-to-first
  paint, but the tracked progress (`report`/`End`) and the delivered result can
  come from different servers — the jarring swap this decision avoids. Rejected
  in favor of priority.
- **Forward the first contributor's `Begin` opportunistically.** Lights the
  indicator a few milliseconds sooner, but `Begin` carries a required `title`, so
  it can surface a non-anchor's label that LSP will not let the bridge amend
  later. Rejected in favor of anchoring `Begin` on the anchor's real `Begin` under
  *preferred*, and a bridge-composed synthetic under *concatenated*.
- **Track every contributor's progress and merge it.** Most information, but
  collapsing N independent `Begin`/`report`/`End` streams (distinct titles,
  percentages) into one coherent indicator is messy and rarely meaningful.
  Rejected in favor of one source's progress under *preferred* and a
  bridge-composed aggregate under *concatenated*.
- **Borrow the anchor's `Begin` under *concatenated*.** Reuses a real, specific
  title for free, but the concatenated lifecycle outlives the anchor's own work
  (it runs until every contributor is collected), so that title goes stale the
  moment the anchor finishes while others are still pending — and LSP forbids
  retitling after `Begin`. Rejected in favor of a synthetic neutral title that
  stays accurate for the whole aggregate; the cost is losing the anchor's
  per-server granularity, accepted because collection `n/m` is the meaningful
  metric for a merge and *preferred* still relays granular single-source progress.
- **Wait for the next-priority candidate after partial data was shown.**
  Avoids delivering incomplete data, but freezes the already-shown
  results until the slower candidate finishes and risks a late swap. Rejected in
  favor of promoting the streamed partials. (An empty anchor that showed
  *nothing* still falls through normally — that is plain latency, not a freeze.)

## Consequences

### Positive

- Client-requested progress (`workDoneToken`) reaches the editor for the first
  time.
- The progress signals stay consistent with the delivered result — no jarring
  swap and no freeze of shown data. Under *preferred* the `report`/`End` track the
  current anchor (normally the winner; after a fall-through the title may be an
  earlier anchor's — see below); under *concatenated* the bridge composes a
  neutral title and `n/m` that describe the aggregate, so the title never goes
  stale mid-flight.
- partialResult streaming becomes possible without mis-translated locations.
- Progress engages only when meaningful: a fast or single-server request with no
  downstream progress shows nothing, so there is no spurious spinner. The `Begin`
  title reflects the priority anchor (or a neutral bridge title), never an
  arbitrary server — apart from the rare lingering-title case after fall-through
  noted below.
- Failure degrades gracefully: if data was shown the editor keeps it and the
  lifecycle terminates cleanly; if not, the request falls through to the next
  server like any empty result.

### Negative

- Requires undoing the deliberate token strip and adding downstream→upstream
  notification routing for client tokens — the real cost, independent of the
  merge policy.
- The aggregation path must move from a single final blob to incremental input to
  support partialResult merging.
- When the winner fails *after* streaming, the delivered result may be incomplete
  (only the streamed prefix); accepted as graceful degradation over freezing or
  swapping.
- If the anchor emits a `Begin` and then loses (returns empty) or dies, its
  `title` lingers on the already-open progress until `End`, since LSP forbids
  retitling. Rare (the anchor both reported progress and failed) and cosmetic;
  accepted. (This is a *preferred*-only edge — *concatenated* never adopts a
  downstream title.)
- Under *concatenated* a downstream's own granular progress (e.g. "indexing
  45%") is discarded in favor of the aggregate `n/m`; a long, dominant
  contributor surrounded by fast ones shows a coarser, possibly stalled count
  instead of its real percentage. Accepted: `n/m` is the honest metric for a
  merge, and a stale borrowed title would be worse.

### Neutral

- Namespacing is unnecessary here (client tokens are already unique), so the
  `ProgressRegistry` of ls-bridge-work-done-progress is not involved; this path
  is distinct from server-declared progress.
- Under fan-out the bridge owns the upstream terminal — relaying the anchor's `End`
  under *preferred*, composing one under *concatenated* and on failure — so
  failure handling changes only that payload, not the mechanism.
- A fast *concatenated* fan-out can briefly flash `Begin`→`End`; editors
  typically debounce short-lived progress, so it is rarely visible.

## Decision–Implementation Gap

**Implemented** (issue #414): the `workDoneToken` path for the **N = 1** relay
and the **`preferred`** strategy with a **single fixed anchor** — the dispatch
mints a per-server bridge token only for the tracked source (sole server at
N = 1, highest-priority *named* anchor at N > 1; a wildcard `Rest` group that
outranks the candidate yields no anchor), routes that downstream's `$/progress`
onto the editor's token, and guarantees a terminal `End` on teardown. Wired for
`textDocument/references`; #446 extends it to the goto family (under #437) and
#448 adds the `workDoneProgress` capability advertisement (under #445; without it
spec-compliant clients never send a token).

**Deferred** (still stripped or unhandled; tracked):

- **`partialResultToken`** is still stripped — only `workDoneToken` is bridged so
  far (the intermediate phase this section already anticipated). #439.
- **`concatenated`** client progress is not built; a concatenated request shows
  no client progress (cost/benefit go/no-go pending). #440.
- **Dynamic fall-through re-anchoring** is not built: the single fixed anchor's
  open `Begin` is closed by the synthetic terminal `End` rather than handed to the
  next named anchor's real `End`. #438.
- **Method coverage** beyond references (and the goto family added by #446) —
  notably the whole-document, multi-region methods (`documentSymbol`, …), which
  fan out to
  several regions per request and so need a request-level shared aggregator, not
  the per-dispatch one. #437.
- **Host-layer** client progress: the host path still strips the token. #441.

The remaining notes below are design-tuning points; the empty-vs-non-empty
fall-through threshold and the `Rest`-never-anchor rule are now **decided** as
described above.

Points still open for the deferred work above (`partialResultToken`,
*concatenated*, and the `Rest`-member refinement):

- `partialResultToken` support depends on the aggregation layer accepting
  incremental input. Until that lands, `partialResultToken` may stay stripped
  while `workDoneToken` is bridged — a valid intermediate phase. Without
  partial-result accumulation a failed candidate always looks empty and falls
  through to the next candidate, which is acceptable.
- Percentage composition under *concatenated* (`n/m`) is a display heuristic, not
  a contract.
- The synthetic `Begin` title the bridge always composes under *concatenated* is a
  presentation choice (e.g. derived from the request method); not specified here.
- The exact "collection is staggered" trigger for the *concatenated* synthetic
  `Begin` (e.g. first result while ≥1 contributor is still pending, or a small
  delay threshold) is an implementation tuning choice; the requirement is only
  that an all-instant fan-out show nothing.
- Ordering `partialResultToken` chunks to match the *concatenated* priority order
  means holding a lower-priority contributor's chunks until the higher-priority
  ones are in — trading streaming latency for ordering fidelity. An
  implementation may relax this if a method's partial results are
  order-insensitive.
- The decided `Rest`-never-anchor rule trades progress visibility for title
  correctness: under *preferred*, if the delivered winner is an unnamed `Rest`
  member doing long work, its own progress is not shown (no safe a priori title).
  A future refinement could surface it once it is the sole active `Rest` member.
