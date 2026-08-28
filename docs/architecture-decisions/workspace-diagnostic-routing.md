# Workspace Diagnostic Routing

**Related Decisions**:
[workspace-symbol-routing](workspace-symbol-routing.md),
[push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md),
[cross-layer-aggregation](cross-layer-aggregation.md)

## Context

`workspace/diagnostic` is document-free and can report files that have never
been opened. Region routing therefore cannot select a language server or
translate an arbitrary virtual report: the current host offset exists only for
an open injection region.

The request's `identifier`, `previousResultIds`, partial-result token, and
work-done token belong to one diagnostic provider. Kakehashi exposes one
provider while querying several downstream providers, so forwarding any of
those values would conflate independent namespaces. In particular, an
`unchanged` report is useful only with the full baseline owned by the producer
that minted its result id.

## Decision

Query one document-free client-workspace connection for every explicitly
configured, runnable server. Start cold connections when needed, admit startup
only while the captured settings generation is current, and accept responses
only from the same live connection generation.

Send each producer an empty `previousResultIds` list and no provider identifier
or progress tokens. Aggregate only full reports and omit downstream result ids
from the upstream response. For the same URI and version, concatenate
diagnostics in stable server-name order. If versions differ, keep the higher
document version. Sort final reports by URI for deterministic output.

Do not expose internal virtual-document reports or related-information links.
Open embedded documents remain covered by `textDocument/diagnostic`, whose
region-aware path can translate current ranges to the host. Real workspace URIs
pass through unchanged, including unopened files.

## Invariants

- A downstream result id or provider identifier never crosses provider
  boundaries.
- An `unchanged` report is never interpreted without the exact baseline it
  names.
- Internal virtual URIs are never returned as editor workspace documents.
- A stale settings snapshot cannot start a removed or reconfigured producer.
- A response from a replaced producer generation is discarded.

## Consequences

### Positive

- Workspace pulls cover capable downstream indexes without depending on prior
  document activity.
- Multiple tools can contribute diagnostics for the same real file.
- Open virtual documents retain the more precise region-aware document-pull
  path.

### Negative

- Every workspace pull asks downstream providers for a full report; result-id
  incremental reuse is intentionally unavailable at the aggregate boundary.
- Partial results and work-done progress are not streamed upstream.
- Workspace reports for internal virtual documents are filtered rather than
  translated because unopened workspace requests have no stable region offset.

## Considered Options

- Forward upstream previous result ids to every producer: rejected because the
  ids are provider-private and can incorrectly authorize `unchanged`.
- Envelope result ids by producer: rejected for now because the protocol gives
  one previous result id per URI while several producers can contribute to that
  URI; a correct envelope also needs cached full baselines for partial producer
  failure and membership changes.
- Return virtual workspace reports directly: rejected because those URIs and
  coordinates are bridge implementation details, not editor documents.
