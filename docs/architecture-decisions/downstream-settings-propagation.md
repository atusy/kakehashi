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

- `build_bridge_client_capabilities` advertises
  `workspace.configuration = true` so spec-compliant downstream servers will
  send `workspace/configuration`.
- Add `settings: Option<Value>` to `BridgeServerConfig` (opaque passthrough;
  kakehashi never interprets the contents) and to `merge_bridge_server_configs`,
  where it **deep-merges** across config layers exactly like
  `initialization_options` (nested objects merge, overlay scalars win), so a
  project layer can override one sub-key without restating the rest.
- After `initialized`, if the server's merged `settings` is non-null, kakehashi
  sends one `workspace/didChangeConfiguration { settings }` so push-model
  servers are configured even before they pull.

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
on a difference it re-stores the cell and sends
`workspace/didChangeConfiguration { settings }` (pull-model servers then
re-request and are answered by (b)). The cell is updated **before** the push so
a pull-model re-pull never races onto the stale value. Unchanged servers get
nothing, so a global reload does not storm every connection.

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

- **Regression risk in (a)↔(b) coupling**: advertising
  `workspace.configuration` may cause a server that previously relied solely on
  `initializationOptions` to start *pulling*. If (b) resolves a section to
  `null` where the server expected a value, that server can *lose* config it
  used to have. (a) must not ship without (b) returning correct values for an
  actually-configured server.
- **`initializationOptions` changes still need a restart**: a server that reads
  a given key only from `initializationOptions` (and ignores
  `didChangeConfiguration`) will not see a runtime change to it until respawn.
- Adds a per-connection settings cell to the bridge pool, shared between the
  reader (serves pulls) and the pool (re-stores on change).
- A failed push (queue full / channel closed) to a push-model server leaves it
  with stale config until it re-pulls or respawns; the cell still holds the
  current value, so a pull-model server self-heals. Guaranteed push delivery
  would need a separate retry baseline and is not implemented.

### Neutral

- `settings` is opaque to kakehashi; correctness of its *contents* is the
  user's responsibility, exactly like `initializationOptions`.
- `settings` joins the per-server deep-merge field list in
  configuration-merging-strategy.

## Decision–Implementation Gap

The following are **deferred** and intentionally out of scope:

- **Upstream pull** (kakehashi → editor `workspace/configuration`): needed only
  to support pull-model editors (e.g. VS Code sends `didChangeConfiguration`
  with no usable `settings` and expects the server to pull). Editors that push
  full settings via `initializationOptions` / `didChangeConfiguration` (e.g.
  Neovim) do not need it. When added, it must be gated on the editor's
  `capabilities.workspace.configuration`.
- **`scopeUri` / multi-root resolution**: `ConfigurationItem.scopeUri` is
  ignored; a single per-server `settings` value answers all scopes.

## Validation

Section resolution cannot be reasoned out in the abstract — real servers differ
on whether `section` is `null` or their own name. (a) is therefore a
**prerequisite** for (b): advertise the capability, log the incoming `items`
(`section` + `scopeUri`) from an actually-configured server, then make
resolution match what was observed. A unit test written on the wrong assumption
passes while the real server gets `null` back. Confirm an actually-configured
server still receives its settings after (a)+(b) land together (the regression
guard above).
