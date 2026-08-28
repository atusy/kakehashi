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

For every explicitly configured, runnable server, query document-free
client-workspace connections that cover every explicit workspace root. A server
that follows workspace-folder changes can use one shared connection; otherwise
each root has its own producer. Start cold connections when needed, admit
startup only while the captured settings generation is current, and accept
responses only from the same live connection generation.

Use the same client-workspace scope proof as `workspace/symbol`: an incapable
`preferSharedInstance` process seeded from a marker root is not a workspace
producer, so the pull uses the client-root fallback. Capture workspace and
settings generations together. Snapshot capture waits for a normal folder
update already underway; a crossed or interrupted workspace generation rejects
the entire aggregate rather than returning reports from mixed scopes.

Send each producer an empty `previousResultIds` list, its own statically or
dynamically declared provider identifier, and no progress tokens. If one server
registers multiple workspace-diagnostic providers, query each provider with its
own identifier. Registrations that share an identifier (including an omitted
identifier) describe the same wire-addressable provider and are queried once.
For a cold connection with no provider snapshot, keep the bounded registration
settle window open through its deadline so sequential post-initialize
registrations join the same first pull.

Treat the planned provider and server sets as one complete aggregate. If any
planned contribution fails, return a JSON-RPC error instead of a partial report;
otherwise the client could replace its previous composite with a response that
silently omitted one producer.
Aggregate only full reports and omit downstream result ids from
the upstream response. For the same URI, concatenate diagnostics in stable
server-name order. Preserve every provider's contribution even when downstream
versions differ: provider requests are not serialized with each reported URI's
close/reopen lifecycle, and the connection-local version counter resets on
reopen, so the numbers do not establish a safe supersession order. Those
versions also belong to Kakehashi's synthetic downstream synchronization stream
rather than the editor's document-version namespace; therefore every upstream
aggregate reports `null`. Sort final reports by URI for deterministic output.

Do not expose reports or related-information links whose exact URI was issued
to that producer generation as an internal virtual document. URI shape alone
is not evidence: a real workspace file that happens to match Kakehashi's
virtual filename pattern still passes through.
Open embedded documents remain covered by `textDocument/diagnostic`, whose
region-aware path can translate current ranges to the host. Real workspace URIs
pass through unchanged, including unopened files.

## Invariants

- A downstream result id or upstream provider identifier never crosses provider
  boundaries; each downstream request uses only that provider's declaration.
- An `unchanged` report is never interpreted without the exact baseline it
  names.
- URIs issued internally to the exact producer generation are never returned
  as editor workspace documents.
- A stale settings snapshot cannot start a removed or reconfigured producer.
- A response from a replaced producer generation is discarded.
- Every report in one aggregate comes from the same stable client workspace;
  a crossed generation invalidates the entire aggregate.
- A failed planned provider or server invalidates the whole pull rather than
  authorizing a partial composite to replace the client's prior diagnostics.

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
