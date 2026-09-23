# Downstream Settings Propagation

**Related**: [configuration-merging-strategy](configuration-merging-strategy.md)

## Context

kakehashi bridges editor requests to downstream language servers (rust-analyzer,
pyright, lua-ls, …). Those servers need workspace configuration — e.g.
`rust-analyzer.cargo.features`, `Lua.diagnostics.globals`. Today kakehashi can
only deliver such config **once**, via `initializationOptions` in the
`initialize` request (`BridgeServerConfig.initialization_options`). There is no
path for:

- a downstream server to **pull** its configuration after startup
  (`workspace/configuration`), nor
- kakehashi to **push** configuration changes to an already-running server
  (`workspace/didChangeConfiguration`).

The editor cannot fill this gap directly: it configures *kakehashi*, not
`rust-analyzer`. A request the editor receives for section `"rust-analyzer"`
has no answer on the editor side. The bridge must therefore own and serve
downstream configuration itself.

kakehashi already maintains a single merged settings tree (see
configuration-merging-strategy): defaults → user file → project file →
`initializationOptions` → `didChangeConfiguration`, exposed at runtime through
`SettingsManager`. The merged tree already carries a per-server map
(`languageServers.<name>`). What is missing is (1) a place in that per-server
config to hold the opaque downstream settings, and (2) the two LSP mechanisms
that move it across the bridge boundary.

Two LSP facts shape the design (LSP 3.18):

- `workspace/didChangeConfiguration` carries the value in a field literally
  named `settings` (`DidChangeConfigurationParams.settings: LSPAny`). Push-model
  servers read it directly.
- `workspace/configuration` carries no such named field: the request is
  `ConfigurationParams { items: ConfigurationItem[] }` where each item has an
  optional `section` / `scopeUri`, and the **response is `LSPAny[]`** — one
  element per item, same order, with the configuration value (or `null`) for
  each. Pull-model servers ignore the `didChangeConfiguration` payload and
  re-request via this round-trip.

A bridge that supports both models must therefore answer the pull request *and*
send the push notification, both sourced from the same merged settings.

## Decision

**Add a per-server `settings` field whose merged value kakehashi propagates to
each downstream server over both `workspace/configuration` (pull) and
`workspace/didChangeConfiguration` (push), keyed off the live merged settings
tree.** Concretely, three coupled changes:

### (a) Advertise the client capability and seed initial settings

- `build_bridge_client_capabilities` advertises `workspace.configuration` so a
  spec-compliant downstream server sends `workspace/configuration` — but **gated
  per-server on that server having `settings` to serve** (`settings.is_some()`,
  evaluated at spawn from the resolved config). Advertising it for a server with
  no `settings` would flip a server configured only via `initializationOptions`
  (today's only mechanism, so every existing setup) into pulling, then answer
  every section `null` — clobbering config it held. The gate makes (a) safe to
  ship alongside (b) without an empirical per-server validation step.
- Add `settings: Option<Value>` to `BridgeServerConfig` (opaque passthrough;
  kakehashi never interprets the contents) and to `merge_bridge_server_configs`,
  where it **deep-merges** across config layers exactly like
  `initialization_options` (nested objects merge, overlay scalars win), so a
  project layer can override one sub-key without restating the rest.
- After `initialized`, kakehashi sends one `workspace/didChangeConfiguration
  { settings }` so push-model servers are configured even before they pull. It
  reads the **live cell**, not the spawn-time value, so a (c) re-propagation
  that landed during the handshake is reflected — the push always agrees with
  what (b) would answer. It fires when the server advertised `configuration`
  (so a clear to `None` during the handshake reaches the server as `null`
  instead of leaving it on stale settings) **or** when the cell now holds
  settings (added during the handshake); when neither holds there is nothing to
  send.

`initialization_options` and `settings` are kept distinct, matching their LSP
roles: `initialization_options` is consumed only at `initialize` time;
`settings` is what flows over `didChangeConfiguration` and answers
`workspace/configuration`. A runtime change to `settings` propagates to running
servers (see (c)); a change to `initialization_options` only takes effect on the
**next** start of that server (it cannot be re-sent post-initialize).

### (b) Answer `workspace/configuration` from the live merged tree

A new arm in the downstream server-request dispatch (`reader.rs`, alongside
`client/registerCapability`, `workspace/workspaceFolders`) handles
`workspace/configuration`. For each requested item it resolves the item's
`section` against this server's merged `settings` and returns an `LSPAny[]` of
the **same length and order** as `items`, using JSON `null` (not an omitted
element) for any miss.

**Section-resolution convention:** the server's merged `settings` object is the
root. An item with `section` omitted/null returns the whole root; a non-null
`section` is a dotted path indexed into the root (`"rust-analyzer.cargo"` →
`settings["rust-analyzer"]["cargo"]`), `null` when absent.

The handler needs two things the dispatch does not carry today:

- **Server identity** — already available: `ServerRequestDeps.server_name` is the
  config-map key.
- **Live merged settings** — *not* available in `ServerRequestDeps`. Wiring a
  settings source (a `SettingsManager` handle or per-connection snapshot) into
  the server-request path is the substantive part of (b), not the section math.

### (c) Re-propagate on merge change, diffing per server

Propagation hooks the single settings-apply choke point (`apply_shared_settings`)
that every reload already funnels through — `didChangeConfiguration`, the
auto-install reload, and initialize. For each **live** downstream connection
kakehashi re-resolves that server's `settings` (through the same wildcard merge
used at spawn) and compares it against the connection's current settings cell;
on a difference it re-stores the cell and, **for a `Ready` connection only**,
sends `workspace/didChangeConfiguration { settings }` (pull-model servers then
re-request and are answered by (b)). The cell is updated **before** the push so
a pull-model re-pull never races onto the stale value. Unchanged servers get
nothing, so a global reload does not storm every connection.

A still-initializing connection is **not** notified — a
`workspace/didChangeConfiguration` before its `initialized` would violate LSP
ordering. Its cell is still advanced, and the post-`initialized` push (a), which
reads the live cell, delivers the latest value once the handshake completes.
That push is queued *before* the connection flips to `Ready`, so the
single-writer FIFO carries it ahead of any `didOpen` a waiter enqueues on
observing `Ready` — the server never processes a document under default config.

The per-connection settings cell *is* the diff baseline: it holds the latest
resolved value (what (b) serves), updated unconditionally on change with the
push sent best-effort afterward — a dropped push self-heals when a pull-model
server re-requests, or on respawn (re-seed + (a) re-push). Not-yet-started
servers need no notification — they pick up the latest merged value at startup
via (a). At initialize time there are no live connections, so the hook is a
clean no-op.

(There is no runtime config-file *re-read* path today; `./kakehashi.toml` and
the user file are read once at startup. Runtime changes arrive via
`didChangeConfiguration`. If a file-watch reload is ever added, routing it
through `apply_shared_settings` extends (c) for free.)

## Considered Options

### Reuse `initializationOptions` for everything
Send all downstream config as `initializationOptions` only. Rejected: it is
delivered once at `initialize` and cannot carry runtime changes; pull-model
servers (which expect `workspace/configuration`) get nothing.

### Forward the downstream `workspace/configuration` up to the editor
When a server pulls, proxy the request to the editor. Rejected: the editor has
no config under the downstream server's section name, the round-trip adds
init-time latency and deadlock risk on the request path, and the request must be
answered promptly. Answering from kakehashi's already-merged local state avoids
all three.

### Always push, never answer the pull (or vice-versa)
Support only one model. Rejected: real servers split across push and pull;
supporting one strands the other. Both are cheap once `settings` exists.

### Broadcast on every reload without diffing
Notify all connections on any merge change. Rejected: floods unaffected servers
and, for pull-model servers, triggers a needless re-pull each. Per-server diff
keeps propagation proportional to actual change.

## Consequences

### Positive

- Downstream servers receive workspace configuration both at startup and on
  runtime change, over whichever mechanism (push/pull) they implement.
- One source of truth: the existing merged settings tree; no parallel config
  store.
- Diff-on-change keeps notifications proportional to real changes.

### Negative

- **A server that starts with no `settings` never pulls, even if settings are
  added at runtime**: the capability is negotiated once at `initialize` and the
  per-server gate evaluates `settings.is_some()` then. If a user later adds
  `settings` for that server, push-model servers still receive it via (c)'s
  `didChangeConfiguration`, but a pure pull-model server will not pull until it
  respawns. This is the deliberate cost of the gate (see (a)) — accepted because
  the alternative (advertising unconditionally) regresses the *common* case:
  every server configured via `initializationOptions` only, which is every
  existing setup, would flip to pulling and be answered `null`.
- **`initializationOptions` changes still need a restart**: a server that reads
  a given key only from `initializationOptions` (and ignores
  `didChangeConfiguration`) will not see a runtime change to it until respawn.
- Adds a per-connection settings cell to the bridge pool, shared between the
  reader (serves pulls) and the pool (re-stores on change).
- A failed push (queue full / channel closed) to a push-model server leaves it
  with stale config until it re-pulls or respawns; the cell still holds the
  current value, so a pull-model server self-heals. Guaranteed push delivery
  would need a separate retry baseline and is not implemented. (The push count
  (c) returns counts only successfully-enqueued notifications, so a drop is
  never reported as delivered.)
- **Accepted narrow race during the handshake window**: path (a) reads the cell
  then sends; a path-(c) store that lands between that read and send (a few-
  hundred-ms window, multithreaded runtime) can leave a push-model server one
  revision behind while the cell — and any pull — already holds the new value.
  The staleness is bounded by *revision*, not time: it persists until the next
  config change re-pushes or the server respawns. This is the same deferred-
  epoch-class race accepted elsewhere (e.g. the pull refresh coverage gate);
  pull-model servers self-heal, and the worst case is bounded staleness, not
  corruption. Fully closing it would require serializing (a) and (c) on a
  per-connection lock.

### Neutral

- `settings` is opaque to kakehashi; correctness of its *contents* is the
  user's responsibility, exactly like `initializationOptions`.
- `settings` joins the per-server deep-merge field list in
  configuration-merging-strategy.

## Decision–Implementation Gap

The following are **deferred** and intentionally out of scope:

- ~~**Upstream pull**~~ (kakehashi → editor `workspace/configuration`):
  **implemented** (#952). Gated on the editor's
  `capabilities.workspace.configuration`, as this decision required. kakehashi
  asks once the handshake completes, again whenever a
  `didChangeConfiguration` carries no usable payload — which is the shape
  pull-model editors send — and again after a `didChangeWorkspaceFolders`
  that moves the selected configuration root. The last is a decision, not a
  protocol requirement: the answer is asked for that root (below), so one read
  for the old root does not describe the new one. It asks only once the reload
  has put the new root in effect, so the answer anchors to it. A folder change
  that keeps the root does not ask, and neither does a reload rejected as
  invalid: that keeps the old root, so an answer would anchor to a workspace
  the editor has left. Asking has a price: a non-empty answer rebuilds the
  settings, so the reload's reparse of open documents and semantic-token
  refresh happen a second time. Accepted, because a root change is rare next
  to an edit.

  The answer is ingested as a push of the same section is (unknown keys
  aside, see configuration-merging-strategy), but retained differently. A push
  accumulates (#734); an answer is the client's whole configuration for the
  scope asked, so it **replaces the previous answer** and lands at its own
  arrival, above older pushes — the newest answer is the newest statement of
  the client's configuration. An answer holding nothing for kakehashi
  withdraws the previous one; `null`, an error, or a timeout is no answer and
  changes nothing. Because the fold is not associative, taking the previous
  answer out cannot be done on the merged snapshot: each applied answer
  rebuilds the settings from the retained layers, re-reading the implicit
  config files as a root change does. That rebuild inherits two gaps #948
  tracks for root changes, now reachable on every pull: an installed parser
  directory appended after an install is dropped if no layer names it (masked
  under the default `searchPaths`, which already include the data directory
  installs go to), and a config file that turned invalid since it was last
  read fails the rebuild, which keeps the settings in effect and discards the
  answer.

  One consequence worth stating for anyone writing an editor integration: a
  field the client answers with an empty container **clears** the layer below,
  exactly as the same spelling would in a config file. LSP carries no
  provenance that distinguishes a value the user authored from a default the
  extension registered, so an extension that registers `{}` as the default for
  a setting would clear it for users who configured nothing. Special-casing it
  is not an option — it would make an intentional clear unspellable through a
  pull-model editor — so register defaults as absent rather than empty.
- **`scopeUri`**: upstream, **implemented for the selected root** (#952).
  The pull's `scopeUri` is the process-wide configuration root — the first
  workspace folder, else `rootUri`, else the deprecated `rootPath` converted to
  a file URI — sent as the client spelled it. It is `null` only when kakehashi
  fell back to its launch directory, which is not a workspace the client can
  answer for. This keeps the invariant that one server process resolves one
  effective settings snapshot: the scope is the root that snapshot is resolved
  for, so no folder's configuration is promoted beyond the scope it already
  had. An answer to a request made for a root the session has since left is
  discarded; the root change pulls again for the new scope.

  On the downstream side `ConfigurationItem.scopeUri` is still ignored — a
  single per-server `settings` value answers all scopes.
- **Per-folder and per-document resolution** stays deferred behind #948's
  retained-layer model, because it breaks the one-snapshot invariant: the layer
  list becomes `(scope, layer)`, and every consumer of the settings has to say
  which scope it asks about. Each has an open question that settings alone do
  not answer:
  - *Which scope a request resolves to* — the containing workspace folder, or
    the document's own URI.
  - *Parser selection* — parsers and queries are loaded once per language name
    for the whole process; two folders naming different parser paths or query
    sets for one language cannot both be honoured without per-scope registries.
  - *Bridge server spawning and reuse* — pooled connections are keyed without a
    configuration scope; per-folder `languageServers` would decide when a
    connection may be shared and when it must be a separate process.
  - *The auto-install gate* — it reads the process-wide `searchPaths`; with
    per-folder paths, whose settings admit an install, and where it goes.
  - *Semantic tokens* — capture mappings and layer aggregation would resolve
    per document, so anything cached across documents would need the scope
    too.

## Validation

Section resolution cannot be reasoned out in the abstract — real servers differ
on whether `section` is `null` or their own name. The handler logs the incoming
`items` (`section` + `scopeUri`) at debug, so the resolution convention can be
confirmed against an actually-configured server. A unit test written on the
wrong assumption passes while the real server gets `null` back; the
dotted-path-into-editor-root convention is chosen because it handles both the
`null` and own-namespace cases, but observing a real server is still the
acceptance check. The per-server capability gate (a) removes the earlier
"(a) must not ship without (b) validated" coupling: a server with no `settings`
is never told to pull, so an incorrect resolution can only affect a server the
user explicitly gave `settings`.
