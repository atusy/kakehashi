# LS Bridge Progress Disconnect Cleanup

**Related Decisions**: [ls-bridge-work-done-progress](ls-bridge-work-done-progress.md), [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md), [ls-bridge-graceful-shutdown](ls-bridge-graceful-shutdown.md)

## Context

Under ls-bridge-work-done-progress the bridge forwards a downstream's
server-declared progress to the editor: a downstream requests a token with
`window/workDoneProgress/create`, the bridge mints a unique upstream token,
forwards the create, and then relays each `$/progress` (begin → report → end)
against that token until a terminating `End` clears the mapping.

A downstream can exit *between* `Begin` and `End` — it crashes, is shut down, or
is respawned while work is in flight. Today the bridge handles that only
*internally*: when the connection's reader task exits, the registry purges the
connection's mappings and the forwarding loop drops their admissions (the
`ForgetWorkDoneProgress` notification removes them from `created_tokens` in
`src/lsp/lsp_impl/lifecycle.rs`). It never forwards a terminating `End` to the
editor.

The consequence is a **dangling progress indicator**: the editor created the
progress on `Begin`, never receives an `End`, and leaves the spinner up
indefinitely. Some editors offer no way to dismiss a progress they believe is
still running. Work-done progress is a strict begin/end lifecycle, and the
bridge currently breaks it on the disconnect path.

## Decision

When a connection's reader task exits, the bridge
**synthesizes a terminating `$/progress` `End`** for every still-open upstream
token that connection owned and forwards it to the editor, in addition to the
existing mapping purge and admission cleanup.

- The set of tokens is exactly the live upstream tokens the registry already
  returns when purging the connection — the same list that drives the
  `ForgetWorkDoneProgress` admission cleanup. The synthetic `End` is emitted for
  each before (or as) that admission is forgotten, so the editor sees one clean
  terminal per token.
- Only tokens the bridge actually minted and forwarded a create for are ended,
  so the editor never receives an `End` for a progress it was never asked to
  create. The existing capability gate (the bridge only forwards a create when
  the editor advertised `window.workDoneProgress`) already guarantees this.
- The synthetic `End` carries no message; the editor needs only the terminal to
  clear the indicator.

This makes every `Begin` the bridge forwards reach a matching `End` even when the
originating downstream dies mid-work. The bridge composes this terminal itself —
the same "bridge owns the upstream terminal" primitive that
ls-bridge-client-progress relies on for client-provided tokens.

## Considered Options

- **Do nothing (status quo).** Simplest, but leaves a dangling spinner whenever a
  downstream dies mid-progress — the defect this decision exists to fix.
  Rejected.
- **Rely on editor-side timeouts.** Work-done progress has no timeout in the LSP
  spec; many editors keep an un-ended progress visible forever. Not a reliable
  cleanup. Rejected.
- **Send the synthetic `End` only on graceful shutdown.** Misses crash and
  respawn — precisely the cases where a downstream is most likely to abandon work
  in flight. Rejected in favour of ending on every reader exit.
- **Synthesize a `window/workDoneProgress/cancel` instead.** Cancel is an
  editor→server signal; the terminal the editor expects for a progress it created
  is an `End` report, not a cancel it would route back to a now-dead connection.
  Rejected.

## Consequences

### Positive

- Every forwarded `Begin` reaches a matching `End`; no dangling progress
  indicators when a downstream disconnects mid-work.
- Cleanup is driven by the live-token list the purge already produces, so it adds
  no new tracking state.

### Negative

- The editor may see an `End` for work that did not truly complete (it ended
  because the server died). Accepted: a clean terminal is strictly better than a
  stuck spinner, and the editor has no way to distinguish "finished" from
  "abandoned" regardless.
- Adds an upstream synthesis step to the forwarding loop: when it processes the
  forgotten tokens it must now emit synthetic `End`s, not just drop admissions.
  The reader's purge path stays decoupled — it already hands the live-token list
  to the loop (via `ForgetWorkDoneProgress`), so no new cross-task coupling is
  introduced; only the loop's existing handler grows.

### Neutral

- Scope is **server-declared** tokens (the ls-bridge-work-done-progress path).
  Client-provided tokens have their own terminal handling under
  ls-bridge-client-progress.
- Shares the synthetic-terminal-`End` primitive with ls-bridge-client-progress;
  both rely on the bridge composing the upstream terminal rather than relaying a
  single downstream's.

## Decision–Implementation Gap

Not yet implemented (tracked in issue #413). Today the reader-exit path purges
the registry mappings and forgets the loop admissions but forwards no terminating
`End`, so an editor still sees a dangling indicator when a downstream dies
mid-progress. This record captures the agreed fix ahead of the change.
