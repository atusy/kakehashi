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

Today the bridge does not forward client-provided tokens to downstreams at all.
The host raw-request path strips them (`strip_progress_tokens`,
`src/lsp/bridge/text_document/host.rs`), and the per-method virtual request
builders construct fresh params with default (empty) progress fields, so no
token is carried either way. A downstream honoring them would stream into the
void, since the bridge discards downstream notifications, and could legally
return an empty final result. The cost is that client-requested progress never
reaches the editor.

## Decision

Stop stripping client-provided tokens (selectively), route the resulting
downstream→upstream `$/progress` and partial-result notifications, and aggregate
them so the editor sees **one coherent lifecycle** that stays consistent with the
result actually delivered.

**Core principle.** Whatever the editor sees stays consistent with the result
actually delivered: the `End` coincides with the result being complete, and no
`report` carries data the editor will not receive. How the title-bearing `Begin`
is produced then depends on the strategy, because the two deliver differently:

- *preferred* delivers a single **winner**, so progress is **anchored** on that
  source: the title is the winner's own `Begin`, and only the anchor's own
  progress is tracked. The winner is found by the **priority walk** — explicit
  `priorities` in order, the wildcard `Rest` resolved first-win by earliest
  non-empty arrival. A candidate that returns empty, or fails before producing
  any data, is no winner (reserved for the delivered source, possibly a promoted
  partial prefix), so the **anchor** (the highest-priority *named* candidate)
  falls through to the next. The bridge never forwards a non-anchor's `Begin` as
  the title — including a `Rest` member's, since which `Rest` member wins is a
  latency race, not knowable in advance. (One documented edge: after a
  fall-through, an earlier anchor's title can linger; see Consequences.)
- *concatenated* merges **all** contributors, so no single source owns the title:
  the bridge **always composes a synthetic neutral `Begin`** (e.g. a
  request-derived title) and reports collection progress,
  **ignoring every downstream's own `$/progress`**. This trades per-server
  granularity for a title
  that stays accurate across the whole aggregate — borrowing one contributor's
  title would go stale the moment that contributor finishes while others are
  still pending (see Consequences).

The lifecycle engages only when there is progress worth showing; otherwise the
request just returns its result (today's behavior, minus the strip).

- **Single downstream (N = 1).** Relay that server's own `Begin`/`report`/`End`
  against the client's original token (client tokens are already unique, so no
  remapping). If it emits none, the editor sees no progress. This holds even when
  the sole server was selected via the wildcard: the `Rest`-never-anchor rule
  disambiguates among *racing* contenders, and a single downstream has none — so
  its real `Begin` is safe to forward.
- **preferred, N > 1.** Preferred short-circuits (it does not wait for the
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
  `Cancelled`). Under *preferred* (and N = 1) the lifecycle tracks a single
  source, so that `End` is the source's real one — or a bridge-synthesized one if
  the source died or was cancelled before sending it. Under *concatenated* the
  bridge composes the whole lifecycle, so the terminal is always its own
  aggregate-timed `End` (fired when all results are collected, or synthesized on
  cancellation). A request that opened no `Begin` emits no `End`. (The per-branch
  rules above are instances of this invariant.)
- **Graceful degradation on committed-server failure.** The branch turns on
  *whether the editor has already been shown data*:
  - *preferred, anchor already streamed partials*: data is on screen, so promote
    the accumulated partials into the delivered result and complete the request.
    `partialResultToken` (data) and `workDoneToken` (progress) are separate
    tokens, so close the *progress* only if a work-done `Begin` is actually
    open — then emit a *synthetic* `End` (the downstream is gone and cannot send a
    real one, the same primitive ls-bridge-progress-disconnect-cleanup uses); an
    anchor that streamed only partial-result data without opening work-done
    progress needs no `End`. Do not wait for another candidate (that would freeze
    the shown data) and do not swap in a different server's result. The result may
    be incomplete; accepted as graceful degradation.
  - *preferred, anchor produced nothing* (empty or failed before any partial
    result): it falls through to the next-priority candidate, exactly as any empty
    result does under preferred. If the spent anchor had opened no `Begin`,
    nothing was shown — ordinary latency, not a freeze or swap — and the new
    anchor's own `Begin` opens the lifecycle. If it had already opened a `Begin`,
    LSP permits only one per token, so that `Begin` stays open (its title lingers
    — see Consequences) and the new anchor's `report`/`End` re-anchor under it;
    should the new anchor provide no real `End` (it completes with no progress
    phase), the bridge synthesizes one at request completion so the lingering
    `Begin` is always closed. This recurses down the priority order; if every
    candidate is exhausted and preferred returns no result at all, the same rule
    applies at request completion — synthesize an `End` if a `Begin` is open,
    otherwise emit none.
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
  later. Rejected in favor of anchoring `Begin` on the anchor (its real `Begin`,
  or a bridge-owned synthetic when it has none).
- **Track every contributor's progress and merge it.** Most information, but
  collapsing N independent `Begin`/`report`/`End` streams (distinct titles,
  percentages) into one coherent indicator is messy and rarely meaningful.
  Rejected in favor of one source's progress under *preferred* and a
  bridge-composed aggregate under *concatenated*.
- **Borrow the anchor's `Begin` under concatenated.** Reuses a real, specific
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
  swap and no freeze of shown data. Under *preferred* the title and `report`/`End`
  track the one winner; under *concatenated* the bridge composes a neutral title
  and `n/m` that describe the aggregate, so the title never goes stale mid-flight.
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
- When the anchor fails *after* streaming, the delivered result may be incomplete
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
- Under fan-out the bridge owns the upstream terminal — relaying the winner's `End`
  under *preferred*, composing one under *concatenated* and on failure — so
  failure handling changes only that payload, not the mechanism.
- A fast *concatenated* fan-out can briefly flash `Begin`→`End`; editors
  typically debounce short-lived progress, so it is rarely visible.

## Decision–Implementation Gap

Not yet implemented (tracked in issue #414); today both client-provided tokens
are stripped before fan-out. Specific points to settle during implementation:

- The empty-vs-non-empty threshold that triggers fall-through must
  **match the existing preferred-strategy empty-result behavior** — a uniform
  fall-through-on-empty across the priority walk, not a per-method exception.
  Align with the preferred strategy; do not invent a new threshold.
- `partialResultToken` support depends on the aggregation layer accepting
  incremental input. Until that lands, `partialResultToken` may stay stripped
  while `workDoneToken` is bridged — a valid intermediate phase. Without
  partial-result accumulation a failed anchor always looks empty and falls
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
- Treating `Rest`-group members as never-anchors trades progress visibility for
  title correctness: under *preferred*, if the delivered winner is an unnamed
  `Rest` member doing long work, its own progress is not shown (no safe a priori
  title). A future refinement could surface it once it is the sole active `Rest`
  member.
