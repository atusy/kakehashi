# Single Resident Multithreaded Tree Worker

| | |
|---|---|
| **Status** | proposed |
| **Date** | 2026-07-18 |
| **Decision-makers** | kakehashi maintainers |
| **Consulted** | Architecture discussion following parser crash-recovery issues #725 and #726 |
| **Informed** | kakehashi users and contributors |

**Related Decisions**:
[per-document-parse-scheduler](per-document-parse-scheduler.md),
[parse-snapshot-architecture](parse-snapshot-architecture.md),
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md),
[replace-tree-sitter-cli-with-loader](replace-tree-sitter-cli-with-loader.md),
[lazy-node-identity-tracking](lazy-node-identity-tracking.md), and
[node-reference-protocol](node-reference-protocol.md).

## Context and Problem Statement

Kakehashi currently loads downloaded Tree-sitter grammar libraries into the LSP
server process and executes their native parser and external-scanner code on a
bounded Rayon pool. The bounded pool protects Tokio's async runtime from
tree-CPU saturation, but it is not a fault boundary: `abort`, `SIGABRT`,
`SIGSEGV`, undefined behavior, or a non-cooperative native hang can still kill
or permanently occupy resources in the LSP server process. Recovering on the
next server start by persisting the set of active parsers is necessarily
reactive, adds cross-session mutable state, and cannot let the current LSP
session log the failure and continue.

Moving only `Parser::parse` behind a thin process RPC would contain the crash
but would make the rest of the architecture worse. A `tree_sitter::Tree` cannot
cross a process boundary as a self-contained value, while injection discovery,
semantic tokens, captures, selection ranges, document symbols, and the node
protocol all traverse that tree. A useful process boundary must therefore own
the complete tree-derived computation, not just the parser call.

The process boundary is also an opportunity to improve performance. The
current architecture already provides a bounded compute pool, incremental
parsing, per-document coalescing, immutable parse snapshots, and parser/query
caches. A worker improves on that baseline only if it preserves internal
parallelism while reducing scheduling transitions, repeated tree walks,
cross-component locking, obsolete work, and unbounded native-call tail
latency.

## Decision Drivers

* A native grammar or external scanner must not terminate the LSP server.
* The server must report a worker crash and continue serving parser-independent
  host-language features in the same session.
* A parser implicated in a worker crash must be quarantined only for the
  current server session; a later server session retries it normally.
* Document text, incarnation, and content version must remain recoverable after
  the worker dies.
* The existing parse-snapshot contract must survive the process boundary: a
  result is immutable, internally consistent, and publishable only for its
  matching document incarnation and version.
* The process boundary must be coarse enough that IPC does not replace local
  tree traversals with a sequence of chatty remote calls.
* The worker must retain bounded multicore execution. "One worker" means one
  child process, not one execution thread.
* Tree work must remain fairly admitted and cancellable without starving the
  LSP control plane or unrelated documents.
* Worker count and routing state should stay fixed until measurements prove
  that multiple worker processes are necessary.
* The migration must preserve current native grammar compatibility, including
  grammars with external scanners.

## Considered Options

1. Keep native Tree-sitter execution in the LSP process and recover on the next
   startup from persisted active-parser markers.
2. Require WebAssembly grammars and run them in an in-process Wasm sandbox.
3. Start one single-threaded resident worker process.
4. Start a variable or per-language pool of worker processes.
5. Start one resident worker process with a bounded internal thread pool.

## Decision Outcome

**Chosen option**: "Start one resident worker process with a bounded internal
thread pool", because it introduces an OS fault boundary without giving up the
current architecture's multicore tree processing, and because one fixed child
avoids document-routing, rebalancing, and cross-worker cache state before those
costs are justified by measurements.

### 1. Parent is the control plane and authoritative input owner

The LSP server process owns the authoritative, reconstructable inputs:

* document URI and client-declared language;
* current `Arc<str>` text;
* document incarnation and monotonic content version;
* workspace/language configuration generation;
* the latest admitted serialized derived results needed for serve-stale and
  parser-independent routing; and
* worker lifecycle, request priority, deadlines, and session quarantine state.

The parent also retains the process-wide installer: source acquisition,
compilation, install deduplication, and the settings transaction mutate global
filesystem/configuration state and are not document-worker responsibilities.

The parent applies LSP edits to its authoritative text before notifying the
worker. It retains enough input state to rebuild the worker after a crash; the
worker is never the only owner of client text. This deliberately keeps worker
state disposable. The duplicated worker-side text is a derived cache maintained
by versioned full-sync and incremental-edit messages, not a second source of
truth.

Host-tier operations defined by parse-decoupled-document-lifecycle remain
entirely in the parent and do not wait for the worker.

### 2. Worker is the tree-derived data plane

Exactly one child process is started lazily when the first tree-dependent
operation needs it. The worker owns:

* dynamic grammar libraries and `tree_sitter::Language` values;
* configured `Parser` instances and parser caches;
* document-local text replicas, incremental edits, and `Tree` values;
* compiled queries and predicate caches;
* injection discovery and injected-layer trees;
* semantic-token, captures, selection-range, symbol, and node computations;
* tree-derived caches and node-identity tracking; and
* the scheduler for all synchronous tree work.

No `Language`, `Parser`, `Tree`, `Node`, query cursor, or other pointer-bearing
Tree-sitter value crosses the process boundary. Handlers that need to traverse
a tree issue a high-level worker operation and receive protocol data such as
ranges, captures, tokens, regions, symbols, or opaque node IDs.

An old node ID presented after a worker restart is unresolved and follows the
node protocol's existing `null` semantics. The parent does not attempt to
reconstruct worker-local node identity.

A worker generation starts in exactly one internal mode. `Serving` mode accepts
only manifest-selected/configured descriptors and may sync documents and serve
tree requests. `Validation` mode accepts one explicitly content-addressed
candidate descriptor, performs only loading, exported-symbol, grammar, and
initial query checks, and rejects document sync and every public tree operation.
It is still supervised and uses hazard/segment attribution, but cannot publish
derived state. A validated process may enter `Serving` only through the
manifest-promotion fence defined below; otherwise it is reaped. Validation is a
mode of the single resident process, not an additional worker.

### 3. One process retains bounded internal parallelism

The worker contains one control/scheduling loop plus a bounded compute pool.
The initial compute budget uses the current tree-CPU policy of reserving cores
for the LSP runtime rather than occupying every available core. The parent
computes and passes the budget at worker startup so that parent and child do not
independently oversubscribe the machine.

The internal scheduler provides:

* at most one parse lifecycle owner per document;
* latest-wins coalescing across queued edits;
* fair admission across documents;
* priority for user-blocking requests over speculative/background derivation;
* a per-document in-flight cap so one injection-heavy document cannot occupy
  every compute thread indefinitely; and
* cooperative cancellation checkpoints within interruptible tree walks.

One worker process is not a promise to serialize all documents. Independent
documents and injection regions may execute on different worker threads within
the shared budget.

Fair admission covers nested work, not only the outer request. Injection and
semantic fan-out must be split into bounded, cancelable chunks and returned to
the scheduler with their document tag; an admitted work unit cannot launch an
unconstrained nested `par_iter` that bypasses admission. For a compute budget
`P > 1`, a document may use all `P` permits only while no competing or
user-blocking work is queued. Once such work is queued, that document receives
no new chunk above `P - 1` running permits, reserving the next available permit
for the competitor. Idle capacity is therefore borrowed only chunk by chunk;
already running native work is not preempted. For `P == 1`, cross-document
concurrency is impossible and the guarantee degrades to priority between bounded
cooperative chunks.

### 4. IPC is versioned and coarse grained

The worker is a hidden mode of the same kakehashi build, but lazy startup never
resolves and executes the install path again. During earliest parent
initialization, kakehashi opens its running image (using `/proc/self/exe` on
Linux where available, and the platform's current-image handle/path otherwise),
materializes a permission-restricted session-private executable snapshot, fsyncs
it, and retains that immutable path/handle for every worker generation. An
in-place package upgrade or deletion of the original path therefore cannot
change the worker build used by an already running parent. If the platform
cannot materialize or execute a snapshot whose embedded build/protocol identity
matches the parent, the tree tier fails closed rather than launching an
unproven image. Normal and abnormal cleanup follows the same parent-lifetime
rules as the worker; stale snapshots are safe startup garbage, not executable
selection inputs.

Parent and worker still perform a build-identity and protocol-version handshake
before accepting document state. A mismatch is a systemic startup failure. The
transport is a framed local byte stream;
the encoding is an implementation choice to be selected by prototype
measurement, but framing, request IDs, version fields, size limits, and unknown
message rejection are protocol requirements.

The state-changing message family is conceptually:

```text
SyncDocument(uri, incarnation, version, language, full_text)
SyncDocumentAck(uri, incarnation, version, worker_generation)
ApplyEdits(uri, incarnation, base_version, version, edits)
ApplyEditsAck(uri, incarnation, version, worker_generation)
ResyncRequired(uri, incarnation, worker_version)
CloseDocument(uri, incarnation)
UpdateConfiguration(configuration_generation, settings)
ConfigurationReady(configuration_generation, worker_generation)
UpdateQuarantine(quarantine_generation, grammar_keys)
QuarantineReady(quarantine_generation, worker_generation)
GrammarMissing(request_id, language, document_context)
LoadNewGrammar(configuration_generation, grammar_descriptor)
GrammarReady(configuration_generation, grammar_key, worker_generation)
```

Tree work uses high-level operations rather than remote Tree-sitter primitives:

```text
RequestContext {
  request_id, worker_generation, uri, incarnation,
  content_version, configuration_generation
}
DeriveSnapshot(context, requested_artifacts)
ResolveNode(context, selector)
NavigateNode(context, opaque_node_id, operation)
RunCaptures(context, query, range)
CancelRequest(request_id, worker_generation)
GrammarHazardStarting(lease_id, request_id, grammar_key)
GrammarHazardArmed(lease_id, worker_generation)
GrammarHazardReleased(lease_id)
NativeSegmentStarting(segment_id, lease_id)
NativeSegmentArmed(segment_id, worker_generation)
NativeSegmentEntered(segment_id, worker_generation)
NativeSegmentExited(segment_id)
NativeSegmentAborted(segment_id)
RequestHazardsReleased(request_id, worker_generation)
RequestTerminal(request_id, worker_generation, error_or_cancel)
```

`DeriveSnapshot` may fuse parse, injection discovery, cache reconciliation, and
eager derived artifacts into one admitted work unit. It returns one internally
consistent response tagged with the exact incarnation, content version,
configuration generation, and worker generation from which it was derived.
Every tree-operation response carries those same guards. The parent admits or
publishes any response only if all live input guards still match. Stale
responses are discarded without mutating parent caches or node state.

The worker also validates the request context against the exact immutable tree
snapshot selected for the operation. A node lookup or walk collects newly
minted identities in a temporary batch, then commits that batch only under a
document-version latch proving that incarnation, content version, configuration
generation, and worker generation did not advance during the walk. A failed
latch discards both identities and response. Applying edits and committing a
node batch therefore cannot interleave into a mixed-version tracker state.

The normal edit path sends incremental edits. Full text is sent on first sync,
after worker restart, or when a base-version mismatch makes incremental replay
unsafe. Large-message copies and serialization cost are measured explicitly;
shared memory is not part of the initial decision and requires separate evidence
if framed streaming proves material.

Parent-to-worker state has one ordered writer and a per-document state machine
for each worker generation:

```text
Unsynced -> SyncQueued(version) -> SyncPending(version) -> Synced(version)
Synced(base) -> EditQueued(base, target) -> EditPending(base, target)
EditPending(base, target) -> Synced(target)
EditQueued(base, target) -> SyncQueued(latest)
```

After startup or restart, the parent first waits for configuration and
quarantine acknowledgments. Every open document then starts `Unsynced` and
enqueues a full sync. `Queued` means the per-document queue still owns and may
replace the frame; `Pending` begins only when the writer atomically claims that
frame for transmission. While a document is `Unsynced`, `SyncQueued`, or
`SyncPending`, concurrent edits update the authoritative parent text. An
unclaimed `SyncQueued` frame is replaced with the latest full text. Once claimed,
its version is fixed and later edits are deferred; after its ack they enqueue a
new latest sync unless a safe retained incremental base is available.

Once `SyncDocumentAck` arrives, the parent may enqueue one incremental chain from
that exact acknowledged version and enters `EditQueued`; writer claim changes it
to `EditPending`. While its ack is pending, later client edits are retained and
coalesced behind that chain; the parent does not send another edit with either
an optimistic or stale base. On
`ApplyEditsAck(target)`, it first enters `Synced(target)`, then sends a composed
incremental from the retained target snapshot to the latest parent version when
that edit history remains available. Otherwise it returns to `Unsynced` and
sends the latest full text. Thus at most one edit chain is unacknowledged per
document without forcing a full sync for every keystroke.

An acknowledgment with the wrong generation, incarnation, base, or target is
discarded and forces `Unsynced`; a worker receiving an invalid base returns
`ResyncRequired` without applying the edit or deriving from partial state, with
the same transition. Close removes every pending state and deferred edit for
that incarnation. Restart creates new per-generation `Unsynced` states rather
than attempting to replay old acknowledgments.

Parent and worker use a separately bounded, bidirectional control transport for
request admission, cancellation, shutdown, liveness, grammar-hazard leases, and
every native-segment frame, including `NativeSegmentExited`. Bulk document state
and derived response payloads use a different bounded transport. A large
full-sync or serialized result therefore cannot sit ahead of an arm or exit
event and falsely consume a native deadline. The control channel has one causal
writer in each direction and is serviced independently of the compute pool.

Within that transport, timer and liveness frames use preallocated high-priority
capacity that ordinary admissions, cancellations, and handshakes cannot consume.
The reserved bound covers at least `Entered` plus `Exited`/`Aborted` for every
compute permit and fixed lifecycle headroom. Per-segment order is preserved:
`NativeSegmentEntered` must be flushed onto the control transport before the
worker enters FFI, and its exit/abort is the next high-priority frame for that
segment. The reader drains this lane first. Prototype measurements establish a
worst-case control delivery bound below the hard deadline with explicit margin;
failure to enqueue or flush within the control-liveness bound is a systemic
protocol failure, not evidence that the grammar exceeded its native budget.

Bulk state writes run on a dedicated async writer and never hold an LSP ingress
ticket while waiting for pipe capacity. Its queue is bounded. Queue replacement
and writer dequeue arbitrate under the same per-document state lock. If
replacement wins while an edit is `EditQueued`, it atomically removes that frame
and transitions to `SyncQueued(latest)`; no acknowledgment for the removed edit
is expected. If dequeue wins, the state is already `EditPending`, the frame must
be sent, and later edits wait behind its exact acknowledgment. The same rule
lets an unclaimed `SyncQueued` payload advance to the latest version but never
mutates a claimed `SyncPending` frame. A wrong or stale acknowledgment takes the
existing `Unsynced` recovery path. Thus backpressure coalesces unsent work without
changing the expected acknowledgment of in-flight work or blocking ingress.
`DeriveSnapshot` and other document work
is admitted only for a version the worker has acknowledged; the worker control
loop applies state messages in stream order before scheduling the corresponding
tree work.

Cancellation is an explicit idempotent protocol operation, not an effect of
dropping the parent-side response future. Supersession, `$/cancelRequest`,
`didClose`, and handler drop send `CancelRequest` for the matching worker
generation. A queued request is removed before execution; a running request
flips the same cancellation token polled by its tree walks and nested fan-out.
Cancellation racing a terminal response may observe either terminal outcome,
but a canceled or stale result cannot populate a cache, mint node state, or be
published in the parent. Canceling an unknown, completed, or prior-generation
request is a no-op. A non-cooperative native call remains governed by its hard
native-call deadline; cancellation alone does not kill the whole worker.

High-level request admission and cancellation share the parent-to-worker control
writer. If cancellation wins before admission is enqueued, the request is never
sent. Otherwise the request frame precedes its cancel frame on that ordered
stream, so an unknown-cancel no-op cannot be followed by first observation of
the canceled request. Duplicate terminal and cancel frames remain harmless.

Cancellation during a handshake has explicit cleanup. A hazard that was armed
but never entered its grammar-backed scope is released; a native segment that
was started or armed but never entered emits `NativeSegmentAborted`; an entered
segment must exit before its hazard can be released. `RequestHazardsReleased` is sent
only after every lease and segment owned by that request is released, exited, or
aborted. It clears only crash-attribution and timer bookkeeping. It does not
remove the response router or complete the public request, because the
independent bulk response may legitimately arrive later. Response routing stays
alive until the guarded bulk response is decoded or an explicit
`RequestTerminal` error/cancellation frame arrives on the control channel.
Worker EOF does not perform hazard cleanup: the still-recorded leases are
intentionally kept for crash attribution, while the supervisor separately fails
the response waiter through its existing reader contract.

### 5. Derived stages are fused when their inputs coincide

The worker boundary must not reproduce the current call graph as multiple IPC
round trips. When parse, injection discovery, bridge-region derivation, and an
eager token computation consume the same `(text, tree, configuration)` tuple,
the scheduler may execute them as one pipeline and publish one derived response.
This preserves cache locality and avoids repeated query execution or store
lookups.

Demand-driven operations such as arbitrary captures queries and node navigation
remain separate messages, but each message performs its complete tree walk in
the worker. The parent never asks for a raw tree and then remotely walks it one
node at a time to implement a single public request.

Every operation that can consume grammar-backed native state has a two-phase
parent-visible boundary. The hazard scope includes dynamic-library loading and
symbol initialization, `Parser::set_language`, query construction, parsing and
external scanners, and every `Tree`, `Node`, query, or node-identity operation
whose raw pointers ultimately depend on the grammar mapping. It is intentionally
broader than the lexical call into `Parser::parse`: native corruption may become
observable only during a later tree walk.

Before the first such dereference, the worker sends `GrammarHazardStarting` with
the actual runtime grammar identity and waits. The parent records the lease in
its session-local active set before replying with `GrammarHazardArmed`; the
worker must not enter the hazard scope without that reply. Each grammar
encountered by a fused host/injection operation is leased separately and remains
active until the complete high-level operation has stopped consuming every
value backed by that grammar. Only then does `GrammarHazardReleased` remove it
from the parent set. A worker that dies after the arm therefore cannot make a
relevant grammar invisible merely because parsing returned before the delayed
failure, while a worker that dies before the arm has not entered that
grammar-backed boundary.

The broad attribution lease is distinct from hard-hang timing. Immediately
before each non-cooperative Tree-sitter FFI segment, such as library loading,
grammar initialization, `Parser::parse`, or a bounded batch of adjacent query-
cursor and `Tree`/`Node` calls, the worker performs the separate
`NativeSegmentStarting`/`NativeSegmentArmed` handshake and emits
`NativeSegmentEntered`; it emits `NativeSegmentExited` immediately on return.
Potentially long query and tree walks are divided into bounded chunks. One
native segment may cover the adjacent FFI calls in a chunk, so traversal does
not add a parent round trip for every match, capture, or node accessor. Chunk
bounds include a maximum number of cursor advances/node operations and a local
CPU budget; the segment contains no scheduler wait, IPC, cache publication, or
response serialization. A hang in any contained FFI call still expires the
batch timer, while normal completion returns to a cancellation/fairness
checkpoint before the next arm. Queue time, fair-scheduler waits, Rust work
between batches, derivation, and response serialization remain inside the
attribution lease but outside the native-segment timer. They use the end-to-end
request deadline and cancellation checkpoints instead of causing a worker kill.

`grammar_key` identifies the resolved artifact and exported grammar, not merely
a configured language alias. Aliases that load the same artifact share a key;
replacing the parser artifact produces a new key. Hazard acquisition and native
segment arming add local control round trips and are included in the performance
acceptance measurements. Replacing them with shared-memory activity slots is a
separate optimization that must preserve the same parent-visible-before-entry
ordering.

The worker never downloads or compiles a missing grammar. It returns
`GrammarMissing`, and the parent's existing global installer deduplicates and
performs that work. A newly installed artifact that has never been mapped in
the current worker may be introduced with `LoadNewGrammar`; the worker
acknowledges its content-derived identity and re-derives the latest acknowledged
versions of documents that recorded the missing dependency.

Replacing an artifact already loaded by the worker is a planned worker restart,
not an in-place reload. The current native loader intentionally keeps libraries
mapped for process lifetime because `Language`, `Parser`, `Tree`, and queries may
retain raw pointers into them, and loading the same canonical path again may
return the old image.

Installed parser artifacts become immutable and content-addressed. The installer
compiles and validates a candidate at a staged same-filesystem path, computes
its cryptographic digest, fsyncs it, and renames it to a path containing that
digest without replacing the currently selected file. A small checksummed
manifest per installer-managed grammar maps its default selection to an artifact
digest and export. This decision introduces a process-shared per-grammar parser-
manifest lock; the current query replacement lock is not reused. Install,
explicit-path import metadata, legacy migration, manifest CAS, and uninstall all
take this lock. Manifest transactions compare the expected revision and publish a new
checksummed revision by fsync-and-atomic-rename followed by directory fsync.
Concurrent parents must re-read and reconcile after a compare-and-swap failure;
they cannot silently overwrite a selection observed after their transaction
started. Serving startup accepts only a complete manifest that names an existing
matching artifact, so a process or power failure observes either the old
committed selection or the new one, never an unrecognized rollback filename or
absent canonical parser.

Manifest-less legacy `parser/<language>.<ext>` installations known from current
configuration are migrated before the lazy resident worker is first enabled.
Under the per-grammar lock, the parent rechecks that no manifest exists and
snapshots the legacy file identity, then imports its bytes without modifying the
legacy path. The single candidate worker validates the exact digest while it is
still the only worker process and before it can serve public tree requests. The
parent reacquires the lock and CAS-creates revision 1 only if the manifest is
still absent and the legacy identity is unchanged. A concurrent migrator that
loses this CAS reaps its candidate and follows the committed winner.

If a previously unknown legacy grammar is discovered after the resident worker
is serving, migration uses the same planned transition as replacement: drain or
cancel, terminate and reap the resident worker, validate with the one fresh
candidate, then commit and enable it. It never starts a second validator beside
the resident worker and never loads unvalidated legacy code into a serving
generation. If import or validation fails, the candidate is reaped, no manifest
is created, and the untouched legacy file remains available for diagnosis or a
later successful migration; it is not loaded directly into the production
worker. Legacy files are not removed by runtime migration or GC.

An arbitrary native path from `LanguageSettings.parser` is compatibility input,
not a path the worker maps directly. At startup and each relevant configuration
transaction, the parent opens the resolved source, copies it to staging while
hashing, verifies that source identity/size/mtime did not change across the
copy, and imports the bytes under their digest. A changed source retries rather
than publishing mixed bytes. The resulting session descriptor contains the
immutable path, digest, and export; aliases with identical bytes and export
share a `grammar_key`. Source mutation cannot change code already mapped by a
worker. A configuration reload or watched source-identity change triggers a new
import and, when the digest changes, the same planned worker transition as an
installed replacement. The external source itself is never modified.

For an installer-managed replacement, the parent stages the immutable candidate,
drains or cancels public tree requests, terminates and reaps the old worker, and
starts the sole worker in non-serving `Validation` mode with the exact digest
without changing the manifest. Only after loading, exported-symbol lookup, and
initial query validation succeed does the parent CAS-commit the expected target
manifest revision. It then constructs a promotion snapshot containing that new
revision plus the unchanged revisions for every other configured grammar and
applies the post-load fence below. On an exact match it advances configuration,
sends `PromoteServing` with the fenced selection and quarantine generations, and
waits for acknowledgment before document sync or public admission. The
candidate's own expected manifest commit is therefore promotion input, not a
revision mismatch. If candidate load fails, the parent first terminates and
reaps that worker, leaves the old manifest
untouched, and may start one rollback worker under the circuit-breaker budget.
If manifest CAS loses to another parent, it also reaps the uncommitted candidate,
re-resolves the winning selection, and starts from that committed state rather
than overwriting it.

This planned transition intentionally makes the tree tier temporarily
unavailable for unrelated documents while the sole worker validates and changes
generation; host-tier service and reader-specific stale/failure contracts remain
available. A transient validator beside the serving worker is not used because
it would violate the fixed one-worker process and CPU/memory budget selected by
this decision. If measured replacement availability later justifies a second
short-lived process, that is a worker-count change requiring an ADR update.

The initial architecture performs no runtime artifact garbage collection.
Immutable files may accumulate; reclaiming them later requires a separate
cross-process live-artifact lease design and cannot infer safety merely from one
parent's worker set. This avoids deleting a DLL mapped by another kakehashi
process. If the old committed selection or candidate cannot load, the tree tier
enters the terminal degraded state rather than running with ambiguous code.

Uninstall is a logical selection transaction, not deletion of a parser file. It
takes the same per-grammar lock and CAS-publishes a revisioned tombstone, then
advances configuration and performs the planned worker transition for affected
documents. Existing workers in other kakehashi processes may finish using their
already selected immutable blob, but future generations and fresh processes see
the tombstone and cannot resurrect the previous digest. Auto-install may replace
the tombstone only through a later authorized install transaction using its
observed revision. Immutable bytes remain untouched during the no-runtime-GC
phase.

The manifest is also the source of truth for parser-facing CLI discovery.
`language list`, parser status, and `language uninstall --all` enumerate logical
grammar names from non-tombstoned manifests, never filenames in the immutable
blob directory. For upgrade compatibility they also scan only the canonical
legacy fixed-path namespace for a grammar that has no manifest revision or
tombstone. List/status report that file as a logical legacy selection without
loading native code. `uninstall --all` takes the per-grammar lock and CAS-creates
a tombstone for it, so a later LSP cannot migrate and resurrect an uninstalled
legacy parser. After migration or uninstall the retained legacy file is ignored.
Query-only languages are enumerated from the query store and unioned separately,
preserving their current status and uninstall behavior without treating a query
as a parser selection.

Every worker-generation startup, including crash recovery, planned replacement,
rollback, and a circuit-breaker half-open probe, re-reads each relevant manifest
revision under its per-grammar lock and snapshots `(grammar, revision,
digest-or-tombstone)` before sending worker configuration. A changed revision
first advances the parent's local configuration generation, then invalidates its
cached descriptor; a tombstone removes the selection and a newer digest is
re-imported and verified. Advancing the publish guard first prevents old-
generation in-flight work from repopulating the cleared cache.

The uncommitted target of `Validation` mode is the sole exception to the serving
selection rule. It is tied to the observed predecessor revision for CAS and is
never part of a serving snapshot until that CAS commits. Promotion then fences
the committed successor revision, so the candidate's expected commit does not
invalidate itself.

Those locks are not held across process startup. After the candidate worker has
loaded and validated its configuration but before `QuarantineReady`, document
sync, or public request admission, the parent re-reads every snapshotted revision
under a deterministic lock order. Any mismatch is a failed startup fence: the
candidate is terminated and reaped, selections are reconciled, and a new
generation is attempted from the latest coalesced snapshot. A fence mismatch is
normal cross-process configuration churn, not a worker failure, and does not
consume the systemic circuit-breaker budget. Reconciliation has its own bounded
backoff so continuous manifest writers cannot create a tight spawn loop; only an
actual startup, handshake, load, or protocol failure is charged. This post-load/
pre-enable fence prevents a running parent from enabling a descriptor superseded
by another process's uninstall or replacement during startup.

Loading the same quarantined `grammar_key` is refused; a genuinely replaced
artifact has a different content identity and may be loaded by the fresh
worker. Missing host and injected grammars use the same event path, so an
injected install cannot bypass quarantine or the configuration-generation
publish guard.

### 6. Crash, hang, and restart policy

The parent supervises the worker as a child process and treats EOF, abnormal
exit, protocol corruption, and a hard native-call deadline as worker failure.
It logs:

* process exit status or terminating signal when available;
* worker generation;
* every admitted but incomplete request ID, document, language, and version;
* the request whose deadline caused a parent-initiated kill; and
* the session quarantine action.

With internal parallelism, an externally observed process crash may leave more
than one acknowledged grammar hazard lease active. The initial safe policy
quarantines the complete set of active `grammar_key` values recorded by the parent. This can
conservatively disable an innocent concurrently active grammar, but only for the
current session. Exact cross-platform identification of the faulting thread
would require a separate crash-reporting design and is not assumed by this
decision. A worker failure with no acknowledged grammar hazard lease is treated as a
worker/protocol failure and does not invent a parser quarantine.

After quarantine, the parent starts a fresh worker with bounded backoff, sends
the versioned quarantine set before enabling document derivation, and full-syncs
all open documents. Restarts are also governed by a per-session systemic-failure
circuit breaker. Quarantine and breaker accounting are separate decisions. Any
active hazard leases at worker loss are conservatively quarantined, but that
fact alone does not classify the cause as native or exempt the restart.

Classification precedence is explicit. A supervisor-observed startup,
handshake, framing/protocol, resync, parent-liveness, or reported Rust panic/
invariant failure is systemic even if hazard leases are active. Otherwise, a
hard `NativeSegment` deadline or an OS crash signal/exception while at least one
hazard lease is active is native-evidenced. Every remaining unattributed or
normally exiting failure is systemic. A native-evidenced failure that newly
quarantines one or more previously allowed grammar keys does not consume the
global budget: quarantine has removed a concrete cause. Its restart remains
rate limited by backoff and the independent native-storm budget below. A
protocol failure may still conservatively quarantine active keys, but it always
consumes the systemic budget.

Three systemic worker-generation failures without an intervening 60 seconds of
healthy service exhaust the budget for the current configuration generation.
Systemic failures include the precedence cases above, failures with no active
grammar lease, and any failure that again involves an already quarantined key,
which demonstrates a quarantine or routing violation. A delayed post-return
native crash is exempt only when its OS failure evidence and still-active lease
satisfy the native-evidenced rule; merely being inside Rust derivation or
serialization under a broad lease is not sufficient.

Native-evidenced recovery has a separate consecutive budget of eight worker
restarts that newly quarantine grammar keys. The count does not decay merely
because failures are spaced apart; only 60 continuous seconds of healthy worker
service resets it. The ninth failure before that reset opens a native-storm
breaker and leaves the tree tier unavailable for the current configuration
generation even though each individual cause was attributable.
After at least 60 seconds without spawning a worker, an explicit parser artifact
or relevant configuration change permits one half-open probe; ordinary requests,
document edits, and new grammar aliases do not. A successful 60-second healthy
interval resets both the native-storm window and the systemic breaker. Thus
quarantine normally preserves service for healthy grammars without allowing an
unbounded sequence of distinct bad artifacts to create a kill/resync loop.

All worker restarts, regardless of classification, also consume a session-wide
long-horizon token bucket with capacity 16. Tokens do not refill with elapsed
wall time or the 60-second fast-budget reset; one token is restored only after
10 continuous minutes of healthy worker service, up to capacity. Restart delay
uses exponential backoff beginning at 250 milliseconds and doubling to a five-
minute cap, and resets only after the same 10-minute healthy interval. A worker
that deterministically fails every 70 seconds therefore drains the bucket rather
than resetting forever.

An empty long-horizon bucket opens the tree-tier breaker independently of the
two fast budgets. After a 10-minute no-spawn cooldown, an explicit parser
artifact or relevant configuration change may borrow one half-open probe; a
failure immediately reopens the breaker, while 10 healthy minutes earns the
first replacement token. Host-tier service and waiter failure contracts remain
available throughout backoff and cooldown.

When the budget is exhausted, the parent stops spawning workers and marks the
tree tier unavailable for that configuration generation. It wakes all tree
waiters through their existing tree-less or method-specific failure contracts,
rejects new tree work without queueing it, logs the terminal degraded state once,
and continues host-tier service. A relevant parser artifact replacement or
configuration generation change permits one half-open probe generation; only
60 seconds of healthy service closes the breaker and resets the full budget.
Ordinary document edits and requests cannot reset it or create a respawn loop.

Every host and injected parser acquisition in the worker
must reject a quarantined `grammar_key`; a document with a quarantined injected
layer degrades only that layer rather than being omitted from resync. Host-tier
service continues throughout recovery. Reload and auto-install events cannot
re-enable the same quarantined artifact, while an actually replaced artifact
has a new key and may be tried. The parent does not persist a `failed_parsers`
set or an active-parse marker across server sessions. A fresh kakehashi process
therefore retries every installed parser.

An end-to-end request deadline starts at request admission and can release the
client without killing the worker. A separate hard native-segment deadline
starts only when the parent receives `NativeSegmentEntered` and ends at
`NativeSegmentExited`, not while an arm frame is queued or in transit and not
for the lifetime of the surrounding grammar hazard lease. The already recorded
lease still provides crash attribution between arm and entry. Missing entry and
control-protocol progress use the supervisor's bounded handshake/liveness
timeout rather than consuming a grammar's native execution budget. A
cooperative timeout that returns control from Tree-sitter does not require a
worker restart. If an entered segment exceeds the hard deadline, the parent
kills and restarts the worker and quarantines the complete acknowledged active
grammar set. This remains correct when one
`DeriveSnapshot` reaches several injected grammars or when unrelated documents
parse concurrently.

The worker belongs to the parent process lifetime. A dedicated worker control
thread, independent of the compute pool, blocks on a parent-liveness handle. EOF
or handle closure terminates the worker process immediately without waiting to
join a potentially hung native compute thread. On macOS and other Unix targets
the baseline mechanism is a close-on-exec liveness pipe whose write end exists
only in the parent. Linux additionally uses `PR_SET_PDEATHSIG` with the standard
post-registration parent-PID race check where available. Windows assigns the
worker to a Job Object with kill-on-job-close and also watches the inherited
parent handle. Process-group membership is used only to target explicit cleanup;
it is not treated as a parent-death signal.

On normal shutdown the parent closes the protocol, waits for a short graceful
exit, then kills and reaps the worker and any owned descendants. On abnormal
parent death, the liveness thread or platform primitive performs the termination
without requiring parent cleanup code to run.

### 7. Relationship to existing decisions

This decision preserves the semantic contracts of parse-snapshot-architecture:
authoritative versioned inputs, immutable internally consistent derivation,
strict publish guards, reader-specific freshness behavior, cancellation, and
bounded tree CPU. It changes more than CPU placement. The parent-held
`ParseSnapshot { text, tree, ... }`, wait-free direct Tree readers, shared
edit-shifted `NodeTracker`, and in-process derivation/cache ownership all require
new serialized or worker-local representations.

Because this record is still proposed, it does not yet supersede the existing
decision. The production cutover must revise parse-snapshot-architecture and
the affected node-identity decisions in the same change, removing or rewriting
the overtaken ownership and local-reader passages according to this repository's
delete-on-supersede convention. Until that coordinated update lands, the
existing in-process snapshot decision remains authoritative.

It preserves the single per-document lifecycle owner from
per-document-parse-scheduler, but places that owner in the worker for
tree-derived work. The parent remains authoritative for document input and
lifetime, so a worker-owned parse task cannot resurrect a closed or reopened
document.

It does not reverse replace-tree-sitter-cli-with-loader. The loader remains
responsible for compiling grammars; runtime loading of the resulting native
library moves into the worker. The existing killable compiler subprocess and
the resident runtime worker are separate lifecycles. Source acquisition,
compilation, and atomic installation remain parent-owned; only the post-install
runtime reload crosses into the worker.

### Consequences

**Positive:**

* A native grammar crash kills disposable derived state, not the LSP server or
  parser-independent host-language functionality.
* Crash detection and logging happen in the current session; restart-time disk
  inference and permanent language-name quarantine are unnecessary.
* One high-level worker request can fuse parse and downstream tree derivation,
  reducing repeated tree walks, scheduling transitions, and shared-store lock
  traffic.
* Stable process-local ownership improves parser, Tree, query, and CPU-cache
  locality while retaining bounded multicore execution.
* A hard worker kill reclaims a native hang that cooperative cancellation
  cannot stop.
* Killing a failed worker releases loaded libraries, trees, caches, and
  allocator fragmentation that the current process-lifetime loader cannot
  reclaim safely.
* Fixed worker count avoids load-balancing and migration state until a measured
  need justifies it.

**Negative:**

* Parent and worker both retain document text so that the worker remains
  reconstructable. Incremental edits limit steady-state copying but do not
  eliminate this duplication.
* IPC framing, serialization, scheduling, and context switches add fixed
  latency, especially for small documents and fine-grained node operations.
* Every tree-dependent handler becomes coupled to the internal worker protocol.
  Parent and worker must evolve their message contracts together.
* A worker crash discards trees and caches for every open document, including
  documents unrelated to the crashing grammar, and recovery causes full parses
  and a temporary CPU/latency spike.
* Planned parser validation/replacement also pauses fresh tree derivation for
  unrelated documents because the architecture never overlaps a validator with
  the serving worker.
* Concurrent native calls make exact crash attribution unavailable without
  additional platform-specific machinery; conservative session quarantine can
  produce false positives.
* A single worker process remains one queueing and memory-failure domain.
  Internal fair admission mitigates head-of-line blocking but cannot provide the
  isolation of multiple processes.
* Process supervision, Windows job/process handling, Unix process groups,
  protocol corruption, restart backoff, and orphan cleanup add operational test
  surface.
* A separate process is a reliability boundary, not automatically a security
  sandbox. The worker initially has the same user privileges as the parent.

**Neutral:**

* Public LSP and kakehashi protocol shapes remain unchanged, but worker
  availability maps through each existing reader contract rather than one
  universal empty result:
  * current-only LSP position/range readers keep their bounded-wait policy where
    applicable and otherwise reject stale/unavailable derivation with
    `ContentModified`;
  * captures range and unavailable captures lineage keep the captures protocol's
    `null` resynchronization signal;
  * whole-document serve-stale readers may use the latest admitted serialized
    artifact retained by the parent and otherwise use their existing method-
    specific empty or `null` fallback;
  * semantic tokens and captures full/delta retain their serve-current wait and
    existing backstop behavior; and
  * a quarantined grammar publishes the same resolved-but-tree-less state as an
    unavailable parser, releasing first-parse waiters without claiming a tree.
* Worker restart permanently invalidates worker-local node IDs. A subsequent
  navigation request returns `null`; the client reacquires an ID through a fresh
  `kakehashi/node` lookup after derivation recovers. Because this is externally
  observable even though it uses an existing protocol result, the initial
  architecture keeps a healthy worker resident and does not recycle it merely
  for idleness or memory pressure.
* Wasm grammars remain a compatible future optimization or stronger sandbox for
  grammars that support them; they are not required by this decision.
* Multiple worker processes remain a possible follow-up, but worker count is not
  user-configurable or automatically scaled in the initial architecture.

### Confirmation

Implementation is accepted only when all of the following hold:

* A child grammar fixture that calls `abort` proves that the LSP process stays
  alive, emits an attributable crash log, quarantines the acknowledged active
  grammar set
  for the session, restarts the worker, and successfully reparses an open
  document in another language.
* Failure injection immediately after `GrammarHazardArmed` proves that the parent
  already holds the crashing grammar key and that the replacement worker refuses
  it for host and injected parser acquisition.
* A saturated bulk full-sync queue proves arm control traffic remains bounded
  and that the native hard deadline begins only after `NativeSegmentEntered`, so
  transport delay alone cannot quarantine a healthy grammar.
* A saturated worker-to-parent derived-result stream likewise proves
  `NativeSegmentExited` and hazard release remain deliverable before their
  deadlines and cannot be trapped behind a large response.
* Saturating ordinary control admission/cancellation traffic proves reserved
  capacity and causal ordering deliver all simultaneously active segment exits
  within the control bound and cannot falsely trigger a native timeout.
* A long, cooperatively chunked derivation may exceed the native-segment
  threshold in total wall time without any individual segment exceeding it; it
  is canceled or rejected through the request contract without killing the
  worker or quarantining its grammar.
* Non-parse hang fixtures cover query-cursor advancement and representative
  `Tree`/`Node` FFI operations inside bounded batches, proving they can trigger
  worker recovery even when no parser call is running. Traversal benchmarks also
  prove control round trips scale with admitted chunks, not matches, captures,
  or individual node accessors.
* A fixture that returns from parsing and fails during query construction or a
  later tree/node walk proves that the grammar remains parent-visible until the
  entire high-level operation leaves its grammar-backed hazard scope.
* A child grammar fixture that hangs proves the hard deadline kills and reaps
  the worker and its descendants, then restores service for non-quarantined
  documents.
* Close/reopen and edit races prove that worker replies with a stale
  incarnation, content version, configuration generation, or worker generation
  cannot publish data, commit node identities, or resurrect a document, for
  snapshots and every demand-driven tree operation.
* Restart tests prove the parent can reconstruct every worker-side document
  from authoritative full text and that pre-restart node IDs resolve to `null`.
* Concurrency tests prove the worker uses more than one compute thread while
  enforcing per-document admission and preserving latest-wins coalescing when
  the configured test budget is at least two.
* A saturated injection fan-out for document A proves that document B starts a
  user-blocking tree operation before A completes. The test covers nested chunks,
  not only two outer requests, and separately covers the one-thread degradation.
* Protocol tests cover truncated frames, oversized frames, unknown versions,
  invalid edit bases, duplicate responses, EOF, and child startup failure.
  Lifecycle tests replace or delete the installed executable after parent
  initialization but before lazy worker start and crash recovery, proving every
  generation executes the retained parent-build snapshot; unverifiable or
  non-executable snapshots fail closed.
  Queue-race tests cover replacement winning before dequeue, dequeue winning
  before replacement, and stale acknowledgments after resync, proving the state
  and expected acknowledgment change atomically.
* Repeated startup, handshake, protocol, and delayed-post-return failures prove
  the restart budget converges to one logged tree-tier-unavailable state, wakes
  waiters, stops respawning, and leaves host-tier service usable. A relevant
  configuration or artifact change permits exactly one half-open probe.
* Three distinct OS-signaled or native-segment-timed-out grammars are each newly
  quarantined without consuming the systemic circuit-breaker budget; a following
  healthy grammar still derives successfully. Re-entry by an already quarantined
  grammar instead consumes the budget and is reported as an invariant violation.
* More than eight distinct native-evidenced grammar failures without an
  intervening healthy interval trip the independent native-storm breaker and
  stop respawning regardless of their spacing; a permitted half-open probe after
  cooldown either restores stable service or returns to the degraded state.
* A deterministic failure every 70 seconds and other slow-frequency patterns
  prove every restart consumes the long-horizon bucket, exponential backoff does
  not reset at the 60-second fast threshold, and the session converges to the
  degraded state instead of resyncing forever.
* A protocol-corruption or reported Rust-panic fixture while a grammar lease is
  active may conservatively quarantine that key but still consumes the systemic
  budget; a signaled native crash with the same lease follows the native-evidence
  exemption instead.
* Install/reload tests cover missing host and injected grammars, concurrent
  install deduplication, cache invalidation, latest-version re-derivation, and
  refusal to load the same quarantined artifact. Same-path parser replacement,
  including migration from the current fixed destination and the loaded-DLL
  case on Windows, proves a fresh worker executes the content-addressed new
  artifact rather than acknowledging a new key while retaining old code.
  Concurrent first-start migration proves exactly one revision-1 manifest wins,
  losers follow it, the legacy file remains intact through every failure point,
  and failed validation leaves no selectable partial migration. Late legacy
  discovery proves the resident is reaped before the sole candidate starts, so
  migration never creates two worker processes or exposes unvalidated code.
  Explicit parser-path tests prove the worker maps an imported immutable copy,
  source mutation cannot alter a live generation, and configuration/source
  refresh adopts changed bytes through a new digest and worker generation.
  Failure injection after every artifact/manifest filesystem step proves the
  next startup selects a complete old or new commit. Post-`dlopen` symbol and
  query-initialization failures prove the candidate worker is reaped before
  the unchanged prior manifest is used or the tree tier degrades.
  Validation-mode tests prove an uncommitted digest cannot receive document or
  public work, its successful CAS is incorporated into the promotion fence, and
  its own expected commit does not invalidate the candidate being promoted.
* Concurrent-parent installer tests prove the process-shared lock and manifest
  revision CAS prevent lost selection updates. A losing candidate follows the
  committed winner, and old immutable artifacts remain present while any other
  process may still map them; runtime GC is absent.
* Concurrent install-versus-uninstall and uninstall-with-live-worker tests prove
  the manifest tombstone wins or loses by revision rather than file timing,
  affected local workers transition generations, other live generations keep
  their mapped bytes safely, and restarting another still-running parent's
  worker revalidates the manifest and cannot select the uninstalled digest.
  A tombstone or replacement committed between the pre-start snapshot and
  enable fence forces candidate reap and reconciliation before public service.
  Several consecutive revision races followed by a stable snapshot eventually
  enable service without consuming the systemic failure budget.
* CLI tests cover migrated selections, tombstones, retained legacy files,
  immutable-blob-only files, and query-only languages, proving list/status and
  `uninstall --all` operate on logical manifest selections rather than storage
  filenames. A fresh pre-migration data directory still lists its canonical
  legacy parsers, and `uninstall --all` tombstones them without loading code.
* Cancellation tests cover queued and running work for client cancellation,
  supersession, handler drop, and `didClose`, including completion races and the
  rule that canceled work publishes and caches nothing.
* Cancellation tests also cover cancellation before request delivery and at
  every hazard/segment handshake phase, proving that no later request executes
  after an overtaking cancel and no lease or timer survives
  `RequestHazardsReleased`.
* A saturated worker-to-parent bulk stream proves hazard cleanup may overtake a
  successful response without removing its router, losing the payload, or
  leaving the public request unresolved.
* Cross-platform lifecycle tests prove normal shutdown and parent `SIGKILL` or
  platform-equivalent abnormal exit do not leave an orphan worker, including
  while a separate compute thread is hung in native code. Tests cover the macOS
  liveness-pipe path, Linux parent-death signal where enabled, and Windows Job
  Object kill-on-close behavior.
* Benchmarks report, separately, IPC enqueue, queue wait, compute, serialization,
  parent resume, bytes transferred, stale-work discard, and cache-hit costs.
* The prototype commits versioned representative corpora and records repeated
  current-main baselines for all of these independent gates:
  * small single-language edit median and p95 derivation latency;
  * injection-heavy sustained-edit p50/p95/p99 latency, obsolete CPU work, and
    total CPU time;
  * concurrent multi-document throughput and the start latency of a small
    user-blocking request behind saturated fan-out;
  * steady-state and peak total parent-plus-worker proportional/resident memory,
    including duplicated document text;
  * worker cold start and full-resync recovery time and bytes copied; and
  * fine-grained node/captures p50/p95 round-trip latency plus the native-call
    arm-handshake cost.
* Before Stage 2 production migration begins, the prototype updates this ADR
  with a numeric non-regression or improvement threshold for every gate above,
  including platform variance and sample-count rules. Passing one improved
  metric cannot waive a failed latency, throughput, fairness, or memory gate.
  The architecture remains proposed and makes no performance-win claim until
  the complete matrix passes.

## Pros and Cons of the Options

### In-process native execution plus restart-time recovery

Keep the current bounded compute pool and make active-parser persistence robust
enough to identify a likely parser after the whole server restarts.

* Good, because Tree-sitter values remain directly accessible with no IPC.
* Good, because it is the smallest change to current execution placement.
* Bad, because a grammar still terminates the LSP session before it can report
  and quarantine the parser.
* Bad, because correctness depends on durable cross-session marker state and
  multi-process locking around a hot parser boundary.
* Bad, because the server cannot hard-kill one non-cooperative native call
  without terminating itself.

### Require Wasm grammars

Compile and execute grammars inside Tree-sitter's Wasm support while retaining
an in-process `Tree`.

* Good, because it provides memory isolation without serializing tree-derived
  operations over IPC.
* Good, because it preserves direct use of the Tree-sitter API.
* Bad, because existing native parser artifacts and some external scanners may
  not be Wasm-compatible.
* Bad, because making Wasm mandatory would narrow kakehashi's current grammar
  compatibility before that compatibility is measured.
* Neutral, because supported grammars may use Wasm later inside either the
  parent or worker without invalidating the chosen ownership model.

### One single-threaded resident worker

Place all tree work behind one actor thread.

* Good, because document and parser state have one simple execution owner and a
  crashing request is exactly attributable.
* Good, because no internal locks are needed for tree-derived state.
* Bad, because it discards current parallel injection and cross-document
  throughput.
* Bad, because one expensive document creates unavoidable head-of-line blocking
  for every tree-dependent request.

### Variable or per-language worker pool

Use multiple child processes and route documents or languages among them.

* Good, because crashes and queues have smaller blast radii.
* Good, because independent processes can use more cores and be recycled
  independently.
* Bad, because document affinity, rebalancing, worker-count policy, duplicate
  grammar/query caches, CPU oversubscription, and migration recovery all add
  mutable coordination state.
* Bad, because no current measurement proves that one internally multithreaded
  process is the bottleneck.

### One resident multithreaded worker

Use one supervised child process with a bounded internal scheduler and compute
pool.

* Good, because it contains native crashes while preserving multicore tree work.
* Good, because stable worker ownership enables coarse derivation requests,
  cache locality, hard cancellation, and disposable tree state.
* Good, because fixed routing minimizes state and leaves measured sharding as a
  compatible later extension.
* Bad, because one crash invalidates all worker-local derived state.
* Bad, because multiple simultaneous native calls make the precise crashing
  grammar ambiguous without conservative quarantine or extra crash reporting.

## More Information

The implementation should proceed in measured stages:

1. Prototype the framed transport, supervision, and one high-level
   `DeriveSnapshot` path behind a development-only switch; record the
   confirmation metrics before selecting an encoding.
2. Build the complete worker-side ownership and high-level reader operations in
   shadow mode while the current in-process path remains the only public source
   of results. Compare serialized worker outputs against the current path; do
   not move the sole Tree away from a parent reader that still needs it.
3. Cut over dynamic loading, parser/Tree/cache ownership, snapshot derivation,
   node tracking, and every tree-dependent handler together behind one guarded
   rollout switch. At this boundary the parent stops serving or traversing a
   local `Tree`; rollback selects the complete legacy path, not a half-migrated
   ownership mix.
4. After failure injection, protocol, compatibility, and benchmark gates pass,
   remove the legacy in-process path and restart-time active-parser persistence.
   Update the existing snapshot, scheduler, and node-identity ADRs in the same
   structural change so their ownership and reader descriptions match reality.
5. Revisit worker sharding only if queue, CPU, or recovery measurements show
   that the fixed worker is the limiting resource.

This ADR is proposed rather than accepted until a prototype satisfies the
confirmation benchmarks and failure-injection tests. Implementation may refine
wire encoding and staging, but changing ownership, worker count, authoritative
state, or crash-quarantine scope requires updating or superseding this decision.
