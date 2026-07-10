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
  per-connection id (phase `Pending`), and acks the downstream immediately.
  The editor-facing `CreateWorkDoneProgress` is **deferred to the token's
  first renderable `begin`** (lazy announcement, below). The downstream is
  acked optimistically (not by relaying the editor's real response) to
  decouple it from editor latency. The bridge advertises
  `window.workDoneProgress` to downstreams **only when the real editor
  advertises it** (the capability merge gates it), so a downstream only sends
  `create` when the editor can accept it — making the optimistic ack sound.
- **`$/progress`** (downstream → bridge notification): the first `begin`
  decides the token's fate (lazy announcement). A `begin` carrying anything
  renderable — a non-empty title, message, or a percentage — **announces**:
  the bridge enqueues the editor-facing create immediately followed by the
  begin on the same FIFO channel, then rewrites and forwards subsequent
  progress. A fully blank `begin` (`{"kind":"begin","title":""}`) **swallows**
  the lifecycle: nothing reaches the editor (a later renderable `begin`
  reusing the token upgrades it to announced). A `report`/`end` before any
  `begin` is dropped — the editor has no progress UI to update. A terminating
  `End` always clears the mapping, forwarded or swallowed.

  Rationale: some downstreams (e.g. basedpyright) declare a fresh token per
  analysis pass — thousands of `create` → `begin("")` → `end` triples per
  minute of typing. Each forwarded create is a full editor round-trip that
  queues ahead of real responses on the shared stdout sink, measurably
  delaying semantic-token replies. Blank begins give the editor nothing to
  render, so eliding those lifecycles is pure win; renderable progress
  (e.g. "Loading workspace") still shows immediately, with no timers or
  reordering risk. The blank-only gate was verified against a production
  capture: all 8,134 storm begins in a one-minute basedpyright session were
  fully blank (empty title, no message, no percentage), while every
  legitimate begin carried a title.
- **cancel** (editor → bridge notification): `RequestIdCapture` intercepts
  `window/workDoneProgress/cancel` (as it already does for `$/cancelRequest`),
  resolves the upstream token to the owning downstream and its original token,
  and forwards the cancel there.

Ordering (the editor must see `create` before that token's `$/progress`) is
preserved because the announce path enqueues create-then-begin back-to-back on
the single FIFO upstream channel and the forwarding loop awaits
`create_work_done_progress` inline before draining the next item — mirroring
the existing `workspace/diagnostic/refresh` forwarding. A re-`create` of a
still-live downstream token only tells the forwarding loop to forget the
evicted upstream token when that token was actually announced.

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
- Per-request blank-progress storms never reach the editor: no create
  round-trips, no `$/progress` floods competing with responses for the
  outbound sink.

### Negative

- Progress whose every `begin` is fully blank is invisible to the editor by
  design. The editor also cannot cancel a never-announced operation (it never
  learns the token) — acceptable, since it has nothing to render a cancel
  affordance on.
- One registry entry per in-flight server-declared progress. Entries are cleared
  on a terminating `End` or on connection teardown. A misbehaving downstream that
  `create`s tokens (or gets cancelled) but never sends `End`, on a long-lived
  connection, accumulates entries until the connection closes — bounded by
  connection lifetime, not individually reclaimed. A per-connection live-token
  cap could bound it further if this ever matters in practice.

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
fan-out) is decided separately in ls-bridge-client-progress.

Terminating a still-open server-declared progress when its downstream
disconnects mid-work (so the editor's indicator does not dangle) is decided in
ls-bridge-progress-disconnect-cleanup.
