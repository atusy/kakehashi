# Run Tree-Derived Computation in One Resident Multithreaded Worker

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

The parent applies LSP edits to its authoritative text before notifying the
worker. It retains enough input state to rebuild the worker after a crash; the
worker is never the only owner of client text. This deliberately keeps worker
state disposable. The duplicated worker-side text is a derived cache maintained
by versioned full-sync and incremental-edit messages, not a second source of
truth.

Host-tier operations defined by
[parse-decoupled-document-lifecycle](parse-decoupled-document-lifecycle.md)
remain entirely in the parent and do not wait for the worker.

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

### 4. IPC is versioned and coarse grained

The worker is a hidden mode of the same kakehashi executable, so parent and
worker versions normally match. They still perform a protocol-version handshake
before accepting document state. The transport is a framed local byte stream;
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
```

Tree work uses high-level operations rather than remote Tree-sitter primitives:

```text
DeriveSnapshot(uri, incarnation, version, requested_artifacts)
ResolveNode(uri, incarnation, version, selector)
NavigateNode(uri, opaque_node_id, operation)
RunCaptures(uri, incarnation, version, query, range)
NativeCallStarting(call_id, request_id, grammar_key)
NativeCallArmed(call_id, worker_generation)
NativeCallFinished(call_id)
```

`DeriveSnapshot` may fuse parse, injection discovery, cache reconciliation, and
eager derived artifacts into one admitted work unit. It returns one internally
consistent response tagged with the exact incarnation, content version,
configuration generation, and worker generation from which it was derived.
The parent publishes the response only if all live input guards still match.
Stale responses are discarded without mutating parent caches or node state.

The normal edit path sends incremental edits. Full text is sent on first sync,
after worker restart, or when a base-version mismatch makes incremental replay
unsafe. Large-message copies and serialization cost are measured explicitly;
shared memory is not part of the initial decision and requires separate evidence
if framed streaming proves material.

Parent-to-worker state has one ordered writer and a per-document state machine
for each worker generation:

```text
Unsynced -> SyncPending(version) -> Synced(version)
```

After startup or restart, the parent first waits for configuration and
quarantine acknowledgments. Every open document then starts `Unsynced`. While a
document is `Unsynced` or `SyncPending`, concurrent edits update the
authoritative parent text and coalesce into the latest full-sync candidate;
incremental messages are not emitted from an unacknowledged base. Once
`SyncDocumentAck` arrives, the parent may send incrementals only from that exact
acknowledged version. A worker receiving any other base returns
`ResyncRequired` without applying the edit or deriving from partial state, and
the parent returns the document to `Unsynced`.

Protocol writes run on a dedicated async writer and never hold an LSP ingress
ticket while waiting for pipe capacity. Its queue is bounded. When backpressure
would retain an obsolete chain of unsent document edits, the parent replaces
that chain with one latest full-sync candidate rather than blocking ingress or
replaying every intermediate version. `DeriveSnapshot` and other document work
is admitted only for a version the worker has acknowledged; the worker control
loop applies state messages in stream order before scheduling the corresponding
tree work.

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

Every entry into untrusted native grammar code has a two-phase parent-visible
boundary. This includes dynamic-library loading and symbol initialization as
well as parsing that can invoke an external scanner. Before entry, the worker
sends `NativeCallStarting` with the actual runtime grammar identity and waits.
The parent records the call in its session-local active set before replying with
`NativeCallArmed`; the worker must not enter native code without that reply. On
normal return, `NativeCallFinished` removes the call from the parent set. A
worker that dies after the arm therefore cannot make the implicated grammar
invisible to recovery, while a worker that dies before the arm has not entered
that native boundary.

`grammar_key` identifies the resolved artifact and exported grammar, not merely
a configured language alias. Aliases that load the same artifact share a key;
replacing the parser artifact produces a new key. The handshake adds one local
round trip to every native entry and is included in the performance acceptance
measurements. Replacing it with shared-memory activity slots is a separate
optimization that must preserve the same parent-visible-before-entry ordering.

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
than one acknowledged native call active. The initial safe policy quarantines
the complete set of active `grammar_key` values recorded by the parent. This can
conservatively disable an innocent concurrently active grammar, but only for the
current session. Exact cross-platform identification of the faulting thread
would require a separate crash-reporting design and is not assumed by this
decision. A worker failure with no acknowledged native call is treated as a
worker/protocol failure and does not invent a parser quarantine.

After quarantine, the parent starts a fresh worker with bounded backoff, sends
the versioned quarantine set before enabling document derivation, and full-syncs
all open documents. Every host and injected parser acquisition in the worker
must reject a quarantined `grammar_key`; a document with a quarantined injected
layer degrades only that layer rather than being omitted from resync. Host-tier
service continues throughout recovery. Reload and auto-install events cannot
re-enable the same quarantined artifact, while an actually replaced artifact
has a new key and may be tried. The parent does not persist a `failed_parsers`
set or an active-parse marker across server sessions. A fresh kakehashi process
therefore retries every installed parser.

An end-to-end request deadline starts at request admission and can release the
client without killing the worker. A separate hard native-call deadline starts
only when the parent records `NativeCallStarting` and sends `NativeCallArmed`.
A cooperative timeout that returns control from Tree-sitter does not require a
worker restart. If an armed call exceeds the hard deadline, the parent kills and
restarts the worker and quarantines the complete acknowledged active grammar
set. This remains correct when one `DeriveSnapshot` reaches several injected
grammars or when unrelated documents parse concurrently.

The worker belongs to the parent process lifetime. Shutdown closes the protocol,
waits for a short graceful exit, then kills and reaps the process (and its process
group or platform equivalent) so orphan workers cannot survive the LSP server.

### 7. Relationship to existing decisions

This decision preserves the versioned-input and immutable-derived-snapshot
model in [parse-snapshot-architecture](parse-snapshot-architecture.md). Once
implemented, it supersedes only that ADR's placement of all tree CPU in an
in-process bounded Rayon pool; the bounded-pool, cancellation, coalescing,
publish-guard, and serve-stale principles move into or across the worker
boundary.

It preserves the single per-document lifecycle owner from
[per-document-parse-scheduler](per-document-parse-scheduler.md), but places that
owner in the worker for tree-derived work. The parent remains authoritative for
document input and lifetime, so a worker-owned parse task cannot resurrect a
closed or reopened document.

It does not reverse
[replace-tree-sitter-cli-with-loader](replace-tree-sitter-cli-with-loader.md).
The loader remains responsible for compiling grammars; runtime loading of the
resulting native library moves into the worker. The existing killable compiler
subprocess and the resident runtime worker are separate lifecycles.

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
* Killing an idle or failed worker releases loaded libraries, trees, caches, and
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

* Public LSP and kakehashi protocol shapes remain unchanged. Only timing around
  worker failure becomes explicitly observable through logs and temporary empty
  results.
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
* Failure injection immediately after `NativeCallArmed` proves that the parent
  already holds the crashing grammar key and that the replacement worker refuses
  it for host and injected parser acquisition.
* A child grammar fixture that hangs proves the hard deadline kills and reaps
  the worker and its descendants, then restores service for non-quarantined
  documents.
* Close/reopen and edit races prove that worker replies with a stale
  incarnation, content version, configuration generation, or worker generation
  cannot publish data or resurrect a document.
* Restart tests prove the parent can reconstruct every worker-side document
  from authoritative full text and that pre-restart node IDs resolve to `null`.
* Concurrency tests prove the worker uses more than one compute thread while
  enforcing per-document admission and preserving latest-wins coalescing.
* Protocol tests cover truncated frames, oversized frames, unknown versions,
  invalid edit bases, duplicate responses, EOF, and child startup failure.
* Cross-platform lifecycle tests prove normal shutdown and abnormal parent exit
  do not leave an orphan worker.
* Benchmarks report, separately, IPC enqueue, queue wait, compute, serialization,
  parent resume, bytes transferred, stale-work discard, and cache-hit costs.
* Against the current in-process baseline, representative small single-language
  edits show no material median regression, while an injection-heavy sustained
  edit workload demonstrates lower obsolete CPU work or lower tail latency.
  Exact thresholds are set before implementation from repeated baseline runs;
  the architecture does not claim a performance win until those thresholds pass.

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

1. Prototype the framed transport and one high-level `DeriveSnapshot` path
   behind a development-only switch; record the confirmation metrics before
   selecting an encoding.
2. Add supervised self-reexecution as a hidden tree-worker mode and move dynamic
   loading, parser ownership, parsing, injection discovery, and snapshot
   derivation into it.
3. Move remaining tree readers (semantic tokens, captures, selection ranges,
   symbols, and node operations) behind high-level worker requests.
4. Remove native grammar loading and tree-CPU execution from the parent, then
   remove restart-time active-parser persistence after crash/restart tests prove
   the new boundary.
5. Revisit worker sharding only if queue, CPU, or recovery measurements show
   that the fixed worker is the limiting resource.

This ADR is proposed rather than accepted until a prototype satisfies the
confirmation benchmarks and failure-injection tests. Implementation may refine
wire encoding and staging, but changing ownership, worker count, authoritative
state, or crash-quarantine scope requires updating or superseding this decision.
