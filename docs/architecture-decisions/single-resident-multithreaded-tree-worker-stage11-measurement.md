# Single Tree Worker Stage 11 Hard-Deadline Measurement

## Scope

Stage 11 exercises the existing process-level recovery deadline against a
deterministic non-cooperative worker hang. The E2E fixture hangs the worker while
synchronizing a Rust document, after an unrelated Lua document has already been
opened and served.

The deadline override is available only in E2E builds and can be restricted to
one URI suffix. Production keeps the 60-second process recovery deadline. This
avoids accidentally applying the short injected deadline to healthy recovery
work, which an intermediate test exposed as a restart storm.

## Method

The E2E test uses one worker with four compute threads:

```text
hung request deadline = 250 ms
hung URI = file:///hung.rs
implicated grammar = rust
surviving open URI = file:///survives-hang.lua
```

The fixture creates a marker and parks forever when the worker first receives
the matching Rust synchronization request. The client deadline must terminate
that worker generation. The supervisor then classifies a timeout with an
implicated grammar as native-evidenced, quarantines that grammar for the
session, starts a new generation, and reconstructs all eligible open documents
from parent-owned full text.

## Result

The focused debug-build E2E run completed in 8.56 seconds and recorded:

```text
restarted worker generation 2 and full-resynced 1 open documents
(bytes=23 recovery_ms=856)
```

The test also proved that:

* the LSP parent remained alive;
* the timed-out generation was reaped;
* the Rust grammar identity was session-quarantined;
* the already-open Lua document was reconstructed in the new generation;
* a subsequent Lua semantic-token request succeeded; and
* the tree tier did not exhaust or trip its systemic restart budget.

## Findings

### A timeout must be scoped to the implicated operation

An intermediate fixture shortened every worker request to 250 ms. Recovery then
subjected healthy catalog and full-resync operations to the same artificial
deadline and took roughly 42 seconds through repeated restarts. Restricting the
override to the hung URI made the same proof complete in 8.56 seconds. Deadline
policy is therefore part of failure attribution, not merely a global transport
constant.

### Existing recovery can reconstruct unrelated state

The worker is disposable: after a hard kill, the parent can recreate an eligible
open document from full text and continue serving it. The quarantined Rust
document is deliberately excluded from resync, while the unrelated Lua document
is restored.

### The full native-attribution gate is still open

This fixture hangs at the worker request boundary and proves timeout, kill,
classification, quarantine, restart, and reconstruction. It does not yet prove
the ADR's `GrammarHazardArmed`, `NativeSegmentEntered/Exited`, saturated control
lane, delayed post-return crash, or descendant-process cleanup contracts.

## Decision

Keep the one-worker hard-deadline recovery path and its independent
native-evidenced breaker classification. Stage 11 closes the basic
non-cooperative hang/recovery proof but does not accept the ADR or authorize
legacy removal.
