# LS Bridge Work Done Progress

**Related Decisions**: [language-server-bridge](language-server-bridge.md), [ls-bridge-message-ordering](ls-bridge-message-ordering.md), [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md)

## Context

Downstream language servers report long-running work to the editor via
work-done progress: they request a token with `window/workDoneProgress/create`,
stream `$/progress` notifications (begin/report/end) against it, and the editor
may abort with `window/workDoneProgress/cancel`.

The bridge multiplexes several downstreams onto one editor connection. Each
downstream mints progress tokens independently, so two downstreams can choose
the *same* token value (e.g. the integer `1`). Forwarding tokens verbatim would
make distinct progress operations collide in the editor (one downstream's
`end` would dismiss another's progress; a cancel would hit the wrong work).

Before this decision the bridge silently acked `window/workDoneProgress/create`
and dropped every `$/progress`, so server-declared progress never reached the
editor at all.

## Decision

Remap **server-declared** progress tokens through a shared `ProgressRegistry`
so client–kakehashi–downstream stays consistent and collision-free:

- **create** (downstream → bridge request): the reader mints a unique upstream
  token `kakehashi/bridge/progress/{N}`, records the mapping keyed by a
  per-connection id, acks the downstream immediately, and forwards a
  `CreateWorkDoneProgress` for the editor. The downstream is acked optimistically
  (not by relaying the editor's real response) to decouple it from editor
  latency; the editor advertises `window.workDoneProgress` downstream (forwarded
  capabilities), so a downstream only reaches here when the editor supports it.
- **`$/progress`** (downstream → bridge notification): the reader rewrites the
  token to the mapped upstream token and forwards it; a terminating `End` clears
  the mapping.
- **cancel** (editor → bridge notification): `RequestIdCapture` intercepts
  `window/workDoneProgress/cancel` (as it already does for `$/cancelRequest`),
  resolves the upstream token to the owning downstream and its original token,
  and forwards the cancel there.

Ordering (the editor must see `create` before that token's `$/progress`) is
preserved because both flow through the single FIFO upstream channel and the
forwarding loop awaits `create_work_done_progress` inline before draining the
next item — mirroring the existing `workspace/diagnostic/refresh` forwarding.

Mappings are purged when a connection's reader task exits (crash, shutdown,
respawn), so dead entries never leak and a later cancel cannot route to a dead
writer.

## Considered Options

- **Forward tokens verbatim** — simplest, but breaks on cross-downstream token
  collisions, the exact failure this bridge must avoid. Rejected.
- **Relay the editor's real create response to the downstream** — spec-faithful,
  but couples the downstream to editor round-trip latency for no practical gain
  (the editor already advertises support downstream, so rejection is a
  non-issue). Rejected in favour of optimistic ack.
- **Remap client-provided `workDoneToken`/`partialResultToken` too** — see gap.

## Consequences

### Positive

- Concurrent downstreams report progress without collisions.
- Server-declared progress reaches the editor at all (previously dropped).
- Cancel is routed to the correct downstream with its own token.

### Negative

- One registry entry per in-flight server-declared progress; bounded and purged
  on End / connection teardown.

### Neutral

- The bridge owns the upstream token namespace (`kakehashi/bridge/progress/*`),
  distinct from the parser-install tokens (`kakehashi/install/*`).

## Decision–Implementation Gap

Scope is **server-declared** tokens only. Progress reported against a
*client-provided* `workDoneToken`/`partialResultToken` (carried in a
client-initiated request) is not remapped — those tokens are already unique
because the client mints them — and such `$/progress` is currently **dropped**,
not passed through, to avoid duplicates under request fan-out (one client
request → multiple downstreams). Passing them through (deduplicated across
fan-out) is possible future work.
