# Downstream Diagnostic Refresh Handling

**Related Decisions**:
- [push-propagation-diagnostic-forwarding](push-propagation-diagnostic-forwarding.md) — The two-path (push cache + client pull) diagnostic model this trigger plugs into
- [cross-layer-aggregation](cross-layer-aggregation.md) — How virt/host layers combine into one published result
- [host-document-bridge](host-document-bridge.md) — Host-layer (`_self`) participation in the pull
- [ls-bridge-message-ordering](ls-bridge-message-ordering.md) — Per-URI ordering between the refresh-triggered pull and edits

## Context

A downstream language server can send `workspace/diagnostic/refresh` — a
server→client request — to tell kakehashi (its client) "my diagnostics may have
changed; re-pull them." In the pull-diagnostics model a refresh comes from a
pull-capable server — a push-driven server simply pushes a fresh
`publishDiagnostics` instead — so receipt normally indicates the sender is
pull-capable.

kakehashi reaches the editor through the two paths of
push-propagation-diagnostic-forwarding:

- **Path A — proactive publish**: a per-host merge cache that kakehashi pushes to
  the editor via `textDocument/publishDiagnostics`. Pull-driven downstream servers
  contribute to it only when kakehashi *pulls* them, gated per language by the
  `pullFallback` toggle on `textDocument/publishDiagnostics`.
- **Path B — client pull**: kakehashi answers the editor's
  `textDocument/diagnostic`, live-pulling pull-driven downstream.

Two forces shape how a downstream refresh should propagate:

- **Completeness of a Path A publish.** A pull-only downstream server never
  pushes, so its diagnostics enter a Path A publish *only* via a kakehashi pull.
  If kakehashi publishes at all, the pull is load-bearing — it is the sole way
  those diagnostics appear in the pushed set, not redundant work.
- **Client pull behavior is largely unknowable.** Whether the editor pulls
  diagnostics at all — and on what cadence — is client-side configuration that LSP
  does not advertise; kakehashi cannot know it. `workspace.diagnostics.refreshSupport`
  is the one reliable signal, but it states only that the client *accepts* a
  `workspace/diagnostic/refresh` (so forwarding one will not leak a pending-request
  entry); a client that is actually pull-tracking then re-pulls, but a client with
  pulling disabled may treat the refresh as a no-op. Diagnostic delivery must
  therefore not silently depend on the editor choosing to pull.

The prior behavior forwarded a downstream refresh to the editor unconditionally
(`src/lsp/lsp_impl/lifecycle.rs`, the `UpstreamNotification::DiagnosticRefresh`
arm) and never re-pulled — so a push-reliant editor saw nothing from the refresh,
and a non-refresh-capable editor was sent a request it would silently ignore
(leaking a tower-lsp pending-request entry, the same hazard
push-propagation-diagnostic-forwarding's republish already guards against).

## Decision

React to a downstream `workspace/diagnostic/refresh` with **three decoupled
rules**. The pull decision is deliberately client-independent; only the editor
forward is capability-gated.

### 1. Accept always (return `result: void`)

kakehashi advertises `workspace.diagnostics.refreshSupport = true` to downstream
unconditionally (`src/lsp/bridge/protocol/client_capabilities.rs`) and answers
the request successfully regardless of what it then does. The response carries no
payload, so *accepting* the request is decoupled from *acting* on it — the
downstream is never blocked, independent of config or which editor is attached.

### 2. Pull iff `pullFallback` (client-independent)

On a downstream refresh, kakehashi re-runs the Path A proactive pull-and-publish
for every open document, pulling each injection-language and `_self` host context
whose `textDocument/publishDiagnostics` aggregation has `pullFallback` enabled
(`pullFallback` is resolved per context in `prepare_diagnostic_snapshot`, not once
per document — a document can hold both eligible and ineligible contexts). This is
the **same** proactive pull as the host-event triggers (`didOpen` / `didSave` /
`didChange`) — a fourth trigger on the identical `prepare_diagnostic_snapshot` →
`collect_push_diagnostics` body, so the resulting publish is a complete merged set
(the pull is what places pull-only downstream into it).

The trigger adds **no editor-capability gate** — eligible contexts stay governed
by `pullFallback` and the existing server-selection configuration (a context with
no configured server, empty effective priorities, or `maxFanOut = 0` already
contributes nothing). Same config ⟹ same behavior on every client. Whether the
sender is pull-only or also pushes is not distinguished (LSP cannot declare it,
and the `filter_pull_driven_push_slots` dedup of
push-propagation-diagnostic-forwarding collapses any overlap).

A downstream refresh is **workspace-wide** (it names no URI), so this trigger
re-pulls every open document with a `pullFallback`-eligible downstream, unlike
the per-URI host-event triggers.

### 3. Forward iff the editor is refresh-capable

kakehashi forwards `workspace/diagnostic/refresh` to the editor only when the
editor advertises `workspace.diagnostics.refreshSupport`
(`check_diagnostic_refresh_support`). The gate's solid justification is
leak-avoidance: a non-refresh-capable editor would silently ignore the request and
leak a tower-lsp pending-request entry (the same hazard
push-propagation-diagnostic-forwarding's republish guards against). Its *intended*
effect is that a pull-tracking editor re-pulls (Path B) and so sees every
pull-driven downstream; whether the editor actually re-pulls is its own
(unadvertised) choice — which is exactly why rule 2's push path is not conditioned
on it. This gate is orthogonal to rule 2's *pull*.

Forwarding is scheduled once per upstream workspace connection, not once per
downstream server. The first downstream refresh after idle is the leading edge
and is forwarded immediately. Further downstream refreshes join one trailing
debounce cycle. Its workspace-wide timing policy is configured independently of
languages and downstream servers:

```toml
[features."workspace/diagnostic/refresh"]
debounceMs = 100
maxWaitMs = 1000
```

The shown values are programmed defaults. `debounceMs` of quiet releases the
latest activity, while the anchored `maxWaitMs` prevents a continuously chatty
server from postponing it forever. `maxWaitMs` must be at least `debounceMs`;
invalid updates are rejected as a whole. A cycle snapshots both values when its
leading edge is admitted, so live updates affect the next cycle without moving
an in-flight deadline. If another editor refresh is sent after the latest downstream activity,
that send already provides the required re-pull nudge and the trailing forward is
suppressed. This keeps independent downstream servers on the same workspace-wide
cadence without coupling separate kakehashi workspace connections.

### Why 2 and 3 are decoupled (not "skip the pull when the editor can pull")

Coupling rule 2 to the editor's refresh capability would save one redundant pull
when both fire, but it would make Path A's behavior depend on the client —
exactly what the project's client-independence principle forbids, and the
capability does not even reliably predict whether the editor actually pulls
(auto-pull is an unadvertised toggle). The user expresses their transport
preference through `pullFallback` instead: a refresh-capable editor that relies on
its own pull sets `pullFallback = false`; a push-reliant or non-capable editor
keeps the default `true`. kakehashi never guesses the editor's behavior.

### Resulting matrix

| editor refresh-capable | `pullFallback` | forward to editor | kakehashi pull + publish | editor stays fresh via |
| --- | --- | --- | --- | --- |
| yes | true  | yes | yes | the push (guaranteed); plus its re-pull if it pull-tracks — downstream pulled twice |
| yes | false | yes | no  | its re-pull **iff it actually pull-tracks**; otherwise stale |
| no  | true  | no  | yes | the push (guaranteed) |
| no  | false | no  | no  | **nothing** — stale (config-incoherent; default `true` avoids it) |

## Considered Options

1. **Remove the proactive pull entirely; rely on push forwarding plus the
   editor's own pull.** Rejected: it breaks {push-reliant editor} ×
   {pull-only downstream} — the editor never pulls, the downstream never pushes,
   so nobody triggers the pull and the diagnostics never arrive. The proactive
   pull (`pullFallback`) is the only bridge for that pairing.
2. **Gate the proactive pull on the editor's pull capability
   (`textDocument.diagnostic`).** Rejected: the capability advertises "can pull",
   not "does auto-pull" — an unreliable proxy — and it makes Path A's behavior
   client-dependent, violating client-independence.
3. **Skip the proactive pull when the editor is refresh-capable** (let the
   forwarded refresh's re-pull cover everything). Rejected: it couples the pull
   decision to a client capability, and refresh-capability does not even guarantee
   the editor re-pulls (it only guarantees the request is accepted) — so the push
   path could be dropped for an editor that never pulls. The residual redundancy is
   instead left to the user's `pullFallback` knob, keeping rule 2 client-independent.
4. **Forward the downstream refresh to the editor unconditionally (status quo).**
   Rejected: a non-refresh-capable client is sent a request it ignores, leaking a
   tower-lsp pending-request entry.
5. **Make accepting the downstream refresh conditional on whether kakehashi will
   pull.** Rejected: the response is `result: void`, so accepting costs nothing;
   decoupling acceptance from action keeps the protocol clean and never blocks the
   downstream.

## Consequences

### Positive

- Downstream-initiated diagnostics that arrive on the *downstream's* timeline —
  build/check-triggered, watcher-driven, cross-file — propagate to the editor via
  whichever path it uses, not just on the next host edit.
- The pull decision is deterministic and client-independent: the same
  `pullFallback` config produces the same Path A behavior regardless of which
  editor connects, which keeps it testable and reproducible.
- Reuses the existing Path A machinery — the refresh is just a fourth trigger on
  the same snapshot/collect/publish body.

### Negative (accepted cost)

- **Double pull in matrix row 1** (refresh-capable editor, `pullFallback = true`):
  if the editor actually pull-tracks, the forwarded refresh causes a Path B re-pull
  alongside kakehashi's Path A pull, so the downstream is pulled twice. This is the
  accepted cost of keeping the pull client-independent — Considered Option 3 (skip
  the pull when the editor can pull) would remove it but couple Path A to a client
  capability. A user who
  *knows* their editor relies on its own pull can set `pullFallback = false` to
  drop the second pull; kakehashi never infers this from capability (the editor may
  advertise refresh support yet have auto-pull off, in which case `pullFallback`
  must stay `true` — it gates the `didOpen`/`didChange` triggers too, not just this
  one). Both resulting diagnostic sets are complete and correct; only the
  downstream pull count differs.
- **`pullFallback = false` relies on the editor actually re-pulling** (matrix rows
  2 and 4): with the push path off, a refresh-driven update reaches the editor only
  if it re-pulls in response to the forwarded refresh (row 2) — and not at all when
  the editor is also non-refresh-capable, so nothing is forwarded either (row 4,
  the fully config-incoherent corner). Because kakehashi cannot know whether the
  editor pulls, the default `pullFallback = true` keeps the push path as the
  guaranteed delivery; only a user who knows their editor pull-tracks should set it
  `false`.
- The trigger is workspace-wide, so a single downstream refresh re-pulls every
  open document with a `pullFallback`-eligible context.

### Neutral

- The editor forward is capability-gated exactly like the push-origin republish in
  push-propagation-diagnostic-forwarding.
- Path B (client pull) is unchanged; it is what a refresh-capable editor uses to
  satisfy the forwarded refresh.

## Decision–Implementation Gap

**Implemented**: rules 1 and 3. For rule 1 (accept always), kakehashi advertises
`workspace.diagnostics.refreshSupport = true` to downstream
(`src/lsp/bridge/protocol/client_capabilities.rs`), and the downstream
`workspace/diagnostic/refresh` request handler
(`src/lsp/bridge/workspace/diagnostic_refresh.rs`) acks it with a `null` result
while emitting `UpstreamNotification::DiagnosticRefresh`. For rule 3, the
lifecycle arm routes that notification through the workspace-wide leading +
trailing scheduler, which checks `workspace.diagnostics.refreshSupport` before
admission and sends through the existing detached single-flight path.

**Planned** (this decision):

- Rule 2 — add the downstream-refresh → Path A pull trigger (today a downstream
  refresh never re-pulls).
