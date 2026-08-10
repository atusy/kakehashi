# LS Bridge Client Capabilities Override

**Related Decisions**: [language-server-bridge](language-server-bridge.md), [ls-bridge-work-done-progress](ls-bridge-work-done-progress.md), [configuration-merging-strategy](configuration-merging-strategy.md)

## Context

The bridge computes the client capabilities it advertises to each downstream
server from two layers: a curated baseline, plus fields propagated from the
real editor's capabilities (gated so the bridge never advertises what it
cannot relay). Users had no say in the result.

That became a problem when an advertised capability itself triggers downstream
misbehavior. Motivating case (issue #976): advertising
`window.workDoneProgress` invites basedpyright to stream tens of thousands of
`$/progress` notifications per minute, which both wastes wire and multiplies
the concurrent stdout writes where its unframed-output race lives. The bridge
deliberately refuses to tolerate downstream framing corruption
(fail-fast-and-quote), so the honest lever is to stop inviting the traffic —
and a dedicated config knob per capability does not scale.

## Decision

A per-server `clientCapabilities` config value, deep-merged over the
advertised capabilities as a third, final layer:

```toml
[languageServers.basedpyright.clientCapabilities.window]
workDoneProgress = false
```

- **User wins, applied last.** Merge order is baseline → upstream-editor merge
  → user override, so an upstream-propagated `true` is reliably masked by a
  user's `false`.
- **JSON-layer merge.** The override folds into the serialized `initialize`
  params, not the typed `ClientCapabilities` — a typed round-trip would
  silently drop capability fields the types don't model (the same trap the
  `workspaceEdit` mirror comments in
  `src/lsp/bridge/protocol/client_capabilities.rs` document). Key order is a
  non-issue: serde_json's `preserve_order` is active build-wide, and the
  writer serializes every outbound request from a `serde_json::Value`
  regardless. The merge itself reuses config's `deep_merge_json`, so
  advertise-time and config-layer merge semantics cannot drift apart.
- **Explicit `false` is the only negation.** The deep merge only sets keys:
  TOML cannot write `null`, and a JSON-carried `null` (editor-pushed config)
  is written through as `null` rather than removing the key. Deny a
  boolean-typed capability with `false`; deny an object-typed one via its
  inner boolean (`window.showDocument.support = false`) — `false` in place
  of an object is an invalid shape a strictly-typed server may reject at
  `initialize`.
- **Two fields are protected as post-merge invariants** — enforced on the
  merged result, so no override shape (a non-object subtree, a JSON `null`)
  can bypass them. `general.positionEncodings`: coordinate translation
  depends on UTF-16, and an override there would silently corrupt every
  bridged position; the baseline value is restored with a warning.
  `workspace.workspaceEdit.changeAnnotationSupport`: the upstream mirror
  deliberately withholds it because the typed edit parse drops
  `annotationId`, so re-advertising it would let a downstream's
  `needsConfirmation` edit apply without confirmation; it is removed with a
  warning. Every other field is the user's to override — reducing capability
  is LSP-safe outside the guarded fields, while *adding* capability may
  invite requests the bridge can only fail (documented as sharp, not
  guarded). The guard criterion is unchanged: only fields whose corruption
  would be silent.
- **Initialize-time only.** The override participates in
  `same_launch_config`, so changing it relaunches the server's connection
  like an `initializationOptions` change.

Across config layers the value deep-merges like
`initializationOptions`/`settings` (wildcard `_` combines under a concrete
server's value) — see configuration-merging-strategy.

## Consequences

- Chatty-capability mitigation needs no bridge release per capability: any
  advertised field can be masked per server from config.
- Users can also advertise capabilities the bridge's typed model doesn't know,
  which is deliberate: the override targets exactly the cases the bridge
  didn't anticipate.
- A user override can lie in the *enabling* direction (e.g. advertising
  `workspace.applyEdit` the editor lacks); the bridge then fails those
  requests as it always has for unsupported traffic. This is accepted — the
  guard list is limited to fields whose corruption would be silent
  (`positionEncodings`, `changeAnnotationSupport`).
- One enabling lie is warned rather than failed: forcing
  `workspace.configuration` against the settings-presence gate
  (downstream-settings-propagation). Enabled without `settings`, the server
  flips to pull and every section is answered `null`; disabled with
  `settings`, the server may never read them. The override wins — it is
  user-explicit — but the spawn site names the conflict per server.
- `workDoneProgress = false` stops *server-initiated* progress at the source
  (`window/workDoneProgress/create` is capability-gated). Progress a server
  reports against a client-supplied `workDoneToken` forwarded with a request
  is a separate channel and is unaffected; the motivating basedpyright flood
  is the former.
