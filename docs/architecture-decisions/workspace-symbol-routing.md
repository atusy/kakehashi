# Workspace Symbol Routing

**Related Decisions**:
[ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md),
[execute-command-routing-token](execute-command-routing-token.md),
[wildcard-config-inheritance](wildcard-config-inheritance.md)

## Context

`workspace/symbol` has no document URI. Kakehashi therefore cannot select a
downstream server by language, injection region, or marker-root walk. It also
cannot return a downstream server's lazy `WorkspaceSymbol.data` unchanged:
`workspaceSymbol/resolve` carries only the symbol, so the later request would
not identify the producing server process.

A workspace search may run before any document is open. Restricting it to live
connections would make the same query depend on unrelated editor history.
Conversely, querying every marker-root connection already created by document
routing can search the same client workspace repeatedly and duplicate results.

## Decision

For each configured, runnable server, workspace symbol search uses one
document-free client-workspace connection. It starts that connection when
needed and combines every capable server's result in server-name order. Flat
`SymbolInformation` responses are normalized to `WorkspaceSymbol`, so results
from old and new servers have one response shape.

If a `preferSharedInstance` process was initialized from a document's marker
root and cannot follow workspace-folder changes, search uses the client-root
fallback connection instead. Reusing that marker-rooted process would make the
search scope depend on which document happened to start it first.

Symbols from a resolve-capable server carry an opaque envelope containing the
producer's server name, connection identity, connection generation, and
original `data`. Resolve is sent only to that exact live producer. A missing,
replaced, or reconfigured producer returns the unresolved enveloped symbol
instead of sending opaque process-owned data to another process.

The bridge advertises downstream lazy-location support only when the upstream
editor's `workspace.symbol.resolveSupport.properties` contains
`location.range`. Search returns one final aggregate; partial-result and
work-done tokens are not copied across producers.

One search is bound to one stable client-workspace generation and one settings
generation from producer selection through final aggregation. Snapshot capture
waits for a normal folder update already in progress. If either generation
changes after capture, or an interrupted update leaves the workspace generation
unstable, search returns `null` rather than a partial result from mixed
workspace scopes. After an interrupted update, searches remain inadmissible
until a later complete folder update recycles the client-workspace producers
that may have missed a delta.

## Invariants

> The invariants below are normative; the mechanisms that satisfy them are
> deliberately unspecified.

- A workspace query must not depend on which documents happened to be opened,
  because the method is defined over the client workspace rather than document
  routing state.
- Opaque symbol data must never cross connection generations. A replacement
  process under the same server and root does not own the old process's data.
- One upstream progress token must not be presented as independently owned by
  several downstream producers, because their progress lifecycles can collide.
- Every contribution in one aggregate must describe the same stable client
  workspace and settings generation; a crossed generation invalidates the
  entire aggregate rather than only the producer still in flight.
- A client that cannot resolve `location.range` must not be told downstream
  results may omit it, or it can receive a permanently unusable location.

## Considered Options

- Query only live connections: rejected because results vary with prior
  document activity and omit configured servers on a cold workspace.
- Query every live marker-root connection: rejected because workspace folders
  can overlap, producing duplicate searches and duplicate symbols.
- Re-select a server by symbol URI during resolve: rejected because unresolved
  workspace locations may contain only a URI, and opaque data belongs to a
  process rather than to a language inferred from that URI.

## Consequences

### Positive

- Cold and warm workspace searches cover the same configured server set.
- Lazy resolve remains exact across multiple servers and workspace roots.
- Old flat-result servers compose with newer lazy workspace symbols.

### Negative

- The first search can pay server startup and indexing latency.
- A workspace-folder-incapable shared server can require a separate
  client-root fallback process for document-free workspace requests.
- Search currently returns no streamed partial results or work-done progress.

### Neutral

- Servers configured with no handled document languages can still provide
  workspace-wide policy or index results; spawnability, not language matching,
  defines participation.
