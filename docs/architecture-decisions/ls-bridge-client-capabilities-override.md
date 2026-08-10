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
- **JSON-layer merge, deferred to serialization.** The override folds into the
  serialized `initialize` params, not the typed `ClientCapabilities` — a typed
  round-trip would silently drop fields the types don't model (the same trap
  documented for `workspaceEdit` metadata). The no-override path does not
  round-trip through `serde_json::Value` at all, keeping its serialization
  byte-identical. The merge itself reuses config's `deep_merge_json`, so
  advertise-time and config-layer merge semantics cannot drift apart.
- **Explicit `false` is the only negation.** TOML has no `null`, so keys
  cannot be removed, only set; an explicit `false` is a spec-honest denial.
- **`general.positionEncodings` is protected.** Enforced as a post-merge
  invariant: whatever shape the override takes (an object override, a
  non-object `general` replacing the subtree, a JSON `null`), the baseline
  value is restored with a warning. Coordinate translation depends on UTF-16,
  and an override there would silently corrupt every bridged position. Every
  other field is the user's to override — reducing capability is LSP-safe
  everywhere except this guarded field, while *adding* capability may invite
  requests the bridge can only fail (documented as sharp, not guarded).
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
  (`positionEncodings`).
