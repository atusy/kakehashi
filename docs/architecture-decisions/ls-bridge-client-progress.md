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
downstream may emit `$/progress` and partial results against the same
client-provided token. Forwarding them verbatim duplicates the lifecycle (N
`Begin`, N `End`) and corrupts the indicator; concatenating result chunks
verbatim mis-orders data. Because of fan-out,
**the bridge always composes the upstream terminal itself** — it aggregates N
downstreams and never simply relays one downstream's `End` or final response.

Today the bridge does not forward client-provided tokens to downstreams at all.
The host raw-request path strips them (`strip_progress_tokens`,
`src/lsp/bridge/text_document/host.rs`), and the per-method virtual request
builders construct fresh params with default (empty) progress fields, so no
token is carried either way. A downstream honouring them would stream into the
void, since the bridge discards downstream notifications, and could legally
return an empty final result. The cost is that client-requested progress never
reaches the editor.

## Decision

Stop stripping client-provided tokens (selectively), route the resulting
downstream→upstream `$/progress` and partial-result notifications, and aggregate
them so the editor sees **one coherent lifecycle** whose data-bearing signals
(`report`, the result, the terminal `End`) all come from the delivered server.

**Core principle.** Every *data-bearing* signal the editor sees — each `report`,
the delivered result, and the terminal `End` — must come from the *same server
whose result is actually delivered*, so progress and data never diverge. The
opening `Begin` is exempt: it is a content-free "work has started" signal
forwarded opportunistically from whichever contributor reports first (the winner
is not yet known then) and carries no server-identifying data. A swap can only
occur before any data has been shown; once data is delivered, the lifecycle is
committed.

- **Selector — priority-based.** The tracked and delivered server is the
  *priority winner* of the preferred fan-in, not the first responder.
  Latency-based selection is rejected because it can make progress (the fastest
  server) and delivered data (the priority winner) come from different servers.
- **Begin — opportunistic.** The winner is unknown when `Begin` arrives (`Begin`
  precedes any result), so forward the *first* `Begin` from any contributor to
  light the indicator immediately; gate only `report`/`End` on the
  currently-chosen server.
- **report / End — per aggregation strategy.**
  - *preferred*: forward only the winner's `report`; emit `End` when the
    winner's final response is aggregated; discard other servers' progress and
    results. If the winner's first response is already complete (not partial),
    do not track other servers at all.
  - *concatenated*: keep progress alive until *all* contributors finish
    (no premature `End`); `report` may reflect `n/m` contributors done as a
    percentage; `End` on the last contributor.
- **Graceful degradation on committed-server failure.** The branch turns on
  *whether the editor has already been shown data*:
  - *preferred, winner already streamed partials*: data is on screen, so promote
    the accumulated partials into the winner's result and immediately emit a
    *synthetic* `End` — the downstream is gone and cannot send a real one, so the
    bridge composes the terminal itself (the same primitive
    ls-bridge-progress-disconnect-cleanup uses). Do not wait for another
    candidate (that would freeze the shown data) and do not swap in a different
    server's result. The result may be incomplete; accepted as graceful
    degradation.
  - *preferred, winner produced nothing* (died before any partial result): its
    result is empty, which — exactly as for any empty or absent winner result
    under the preferred strategy — falls through to the next-priority candidate.
    Nothing was shown yet, so this is ordinary request latency, not a freeze and
    not a swap. The opportunistic `Begin` stays open and `report`/`End` re-gate
    onto the new candidate (no new `Begin`); this recurses down the priority
    order.
  - *concatenated*: a failed contributor donates its accumulated partials
    (possibly empty) and the others concatenate as usual; nothing special is
    needed.
- **partialResultToken — translate, then merge.** Partial-result chunks carry
  locations needing the *same* injection offset and URI translation as final
  responses, applied incrementally per chunk through the existing aggregation
  path (which assumes a single final blob today and must accept incremental
  input). *Preferred* streams the winner's translated chunks; *concatenated*
  concatenates all contributors' translated chunks.

The terminal `End` the bridge emits on failure is the same primitive
ls-bridge-progress-disconnect-cleanup uses for server-declared tokens — the
bridge composes the terminal rather than relaying a downstream's.

## Considered Options

- **Keep stripping both tokens (status quo).** Simplest and zero-risk, but
  client-requested progress never surfaces — the gap this decision closes.
  Rejected.
- **Latency-based selector (track the first responder).** Lower time-to-first
  paint, but progress and delivered data can come from different servers — the
  jarring swap this decision avoids. Rejected in favour of priority.
- **Delay `Begin` until the winner is known.** Keeps progress source-consistent
  from the first frame, but defeats the point of progress (no early "something is
  happening" signal). Rejected in favour of an opportunistic first `Begin` with
  source-gated `report`/`End`.
- **Wait for the next-priority candidate after partial data was shown.**
  Avoids delivering incomplete data, but freezes the already-shown
  results until the slower candidate finishes and risks a late swap. Rejected in
  favour of promoting the streamed partials. (An empty winner that showed
  *nothing* still falls through normally — that is plain latency, not a freeze.)

## Consequences

### Positive

- Client-requested progress (`workDoneToken`) reaches the editor for the first
  time.
- The data-bearing progress signals (`report`/`End`) and the delivered result
  always share one source — no jarring swap and no freeze of shown data.
- partialResult streaming becomes possible without mis-translated locations.
- Failure degrades gracefully: if data was shown the editor keeps it and the
  lifecycle terminates cleanly; if not, the request falls through to the next
  server like any empty result.

### Negative

- Requires undoing the deliberate token strip and adding downstream→upstream
  notification routing for client tokens — the real cost, independent of the
  merge policy.
- The aggregation path must move from a single final blob to incremental input to
  support partialResult merging.
- When a winner fails *after* streaming, the delivered result may be incomplete
  (only the streamed prefix); accepted as graceful degradation over freezing or
  swapping.

### Neutral

- Namespacing is unnecessary here (client tokens are already unique), so the
  `ProgressRegistry` of ls-bridge-work-done-progress is not involved; this path
  is distinct from server-declared progress.
- The bridge always composes the upstream terminal `End` and response (it
  aggregates fan-out), so failure handling changes only that payload, not the
  mechanism.

## Decision–Implementation Gap

Not yet implemented (tracked in issue #414); today both client-provided tokens
are stripped before fan-out. Specific points to settle during implementation:

- The empty-vs-non-empty threshold that triggers fall-through must
  **match the existing preferred-strategy empty-result behaviour** — a uniform
  fall-through-on-empty across the priority walk, not a per-method exception.
  Align with the preferred strategy; do not invent a new threshold.
- `partialResultToken` support depends on the aggregation layer accepting
  incremental input. Until that lands, `partialResultToken` may stay stripped
  while `workDoneToken` is bridged — a valid intermediate phase. Without
  partial-result accumulation a failed winner always looks empty and falls
  through to the next candidate, which is acceptable.
- Percentage composition under `concatenated` (`n/m`) is a display heuristic, not
  a contract.
