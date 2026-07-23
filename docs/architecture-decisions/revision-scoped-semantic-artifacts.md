# Revision-Scoped Semantic Artifacts

**Related Decisions**: [parse-snapshot-architecture](parse-snapshot-architecture.md),
[injection-token-cache-reuse](injection-token-cache-reuse.md),
[semantic-token-overlap-resolution](semantic-token-overlap-resolution.md),
[per-document-parse-scheduler](per-document-parse-scheduler.md)

## Context

The parse-snapshot-architecture decision made each parsed document revision an
immutable, internally consistent `(text, tree, language, version)` value. The
semantic-token pipeline does not yet extend that model through its derived data.
After any edit misses the whole-document cache, it reconstructs a flat token
result by walking the host query, discovering and tokenizing injections,
resolving overlaps, encoding LSP positions, and comparing the full result with a
delta baseline. A small edit therefore still pays work proportional to the
document or injection graph rather than to the affected semantic area.

The retained non-worker benchmark contract shows why the next step must target
revision work rather than request suppression. On the 2026-07-23 baseline,
ordinary Rust large-document edit-to-delta responses remain roughly 25--29 ms
and cancellation bursts roughly 130--140 ms. Version-scoped cancellation made
the cancellation-burst control repeatably 10.24% faster (four-pair range
-13.10% to -8.09%) while ordinary typing controls crossed zero. This proves that
retiring obsolete revision work can improve latest-response latency, but does
not show that the current revision's whole-document computation became cheaper.

The editor-facing contract remains strict: every accepted full or delta response
must describe the latest edit. Debouncing current semantic requests, returning
`null`, omitting the newest response, or serving one input version's artifact as
another would make highlighting lag behind typing and is not an acceptable
optimization. The per-document parse scheduler may continue to coalesce obsolete
intermediate parses and derive the newest revision; this decision does not restore
per-edit parse work.

## Decision Drivers

- Make current-revision semantic work proportional to the affected semantic area
  where a correctness boundary can be proved.
- Preserve latest-edit freshness, request cancellation, and exact LSP full,
  delta, and range behavior.
- Reuse the immutable ownership and version identity already established by
  parse-snapshot-architecture instead of introducing a second mutable document
  owner.
- Share identical current-snapshot computation without making one consumer's
  cancellation abort work still required by another consumer.
- Keep a byte-for-byte whole-document oracle and automatic fallback for every
  unclassified query or invalidation case.
- Bound retained recomputable state by both document count and bytes.
- Deliver the architecture as small, independently reviewable and measurable
  changes.

## Decision

Introduce a revision-scoped, immutable `SemanticArtifact` derived from one
`ParseSnapshot`. Initially it wraps the existing whole-document computation
without changing output. Later stages may store persistent layer-local chunks
and structurally share safe chunks with an adjacent revision.

The target data flow is:

```text
Document input revision
  -> ParseSnapshot
  -> immutable injection/layer forest
  -> layer-local raw-token chunks
  -> overlap-resolved token tree
  -> full / delta / range views
```

This is an ownership and reuse boundary, not permission to serve partial work.
An artifact becomes visible only after all data needed for its declared state is
complete and its identity has been checked.

### 1. Identity and ownership

Every artifact key contains all inputs that can change semantic output:

- document URI and open `incarnation`;
- `ParseSnapshot.parsed_version`;
- semantic settings and query generation, including injection queries;
- the session-constant client capabilities that affect overlap and multiline
  token shaping; and
- the semantic legend generation when capture-to-token mapping changes.

The `ParseSnapshot` owns a generation-aware `SemanticArtifactSlot`. A settings,
query, legend, or capability generation can change without publishing a new
parse snapshot, so the slot is replaceable by key rather than single-assignment
for the snapshot's entire lifetime. Installing a new key cancels and detaches the
old key's producer; requests already holding its completed `Arc` may finish but
cannot publish it as current.

Each production **attempt** is lazy and single-assignment, moving from `empty`
to `computing` to `complete`. An attempt carries an identity so only its producer
may complete or remove it. Cancellation, timeout, panic, or another failure
compare-and-removes that attempt back to `empty`; a later request for the same
key can therefore become producer and retry. A partial result is never readable.
Artifact data holds no strong reference back to its snapshot, avoiding a
retention cycle.

Requests clone an `Arc` to the selected snapshot and artifact. The request still
performs the existing final currency check before delivery. Artifact identity
does not replace delivery validation.

### 2. Same-revision sharing and cancellation

Full, delta, range, refresh-driven, and concurrent requests for an identical key
join one artifact computation. Response shaping remains per request: a range
request projects the artifact, a delta request compares with its own accepted
baseline, and a full request materializes the complete LSP array.

Cancellation has two scopes:

- **revision cancellation** is owned by the document version. A newer input
  version or close cancels production for the obsolete artifact;
- **consumer cancellation** detaches only that request. It cancels shared
  production only when no consumer remains and the artifact is not current
  producer-prefetch work.

Thus a cancelled old request cannot consume CPU indefinitely, while one client's
`$/cancelRequest` cannot starve another request for the same current revision.
Semantic artifact identities are never combined across input versions, and the
newest revision always receives a new live cancellation scope. This is compatible
with the parse scheduler dropping obsolete intermediate parse passes before an
artifact exists.

### 3. Persistent representation

The eventual representation is an ordered persistent tree or rope of immutable
token chunks indexed by source byte and line intervals. Chunks exist before and
after overlap resolution. Position shifts after an edit are represented by tree
metadata or lazy offsets, not by rewriting every following token.

The representation must support:

- replacing dirty layer chunks without mutating a prior revision;
- resolving overlaps in the invalidated intervals plus proven boundaries;
- projecting a range without first flattening the whole document;
- producing LSP delta edits directly from changed finalized chunks; and
- flattening byte-for-byte to the existing full result as the oracle.

Introducing chunks does not itself enable reuse. The first chunked stage rebuilds
every chunk and proves that flattening is exact.

### 4. Incremental-safety classes

Tree-sitter `changed_ranges` is a hint, not a correctness boundary. Ancestor
captures and predicates, row shifts, multiline tokens, injection-boundary
changes, combined or nested regions, and overlap winners can change output
outside a raw changed range.

Every query and pipeline stage is classified at load time or conservatively at
execution time as one of:

1. **local** -- invalidation is bounded to changed chunks and their overlap
   boundary;
2. **expanded** -- correctness requires expanding to a syntax ancestor, complete
   injection layer/region, or another declared boundary; or
3. **global/unknown** -- the whole-document oracle must run.

An unclassified construct always selects class 3. A class may serve incremental
results only after its shadow output has matched the oracle byte-for-byte on the
retained corpus and randomized edit sequences.

### 5. Invalidation

Artifact reuse is invalidated as follows:

| Change | Required invalidation |
|---|---|
| document incarnation | all artifacts and delta lineage for the URI |
| content / parsed version | current root changes; only proven chunks may be shared |
| host or injection query generation | affected language/layer chunks, otherwise full |
| semantic settings or legend generation | all affected raw/finalized chunks and views |
| overlap or multiline capability | finalized representation and all shaped views |
| injection identity, boundary, nesting, or combined grouping | complete affected layer/region; full when classification is unknown |

Failed, panicked, timed-out, or cancelled production publishes no artifact. A
request then retries the current key or uses the existing full fallback; it never
converts failure into a completed empty result.

### 6. Retention and pressure

Retention follows reachability and hard budgets:

- a document retains its current artifact root;
- full/delta preserves the existing bounded LRU history for reissued result IDs:
  at most eight accepted baselines and 512 KiB of flat token payload per document,
  while always keeping the newest baseline; a successful read refreshes a
  baseline's recency;
- prior artifact roots may accompany those flat baselines only within the new
  artifact byte budgets. If a root is evicted, the retained flat baseline still
  supports the existing full/delta comparison path; range requests retain no
  history;
- active requests may temporarily retain their exact roots until completion;
- closed incarnations are removed from lookup immediately. Superseded baselines
  follow the bounded LRU policy rather than disappearing immediately, while
  in-flight `Arc`s may finish dropping naturally; and
- cross-revision reusable chunks enter a weighted cache only after both a
  per-document byte limit and a process-global byte limit exist.

Byte accounting includes chunk payloads, indexes, source mappings, and owned
strings, not only vector capacity. Under pressure, evict recomputable prior roots
and chunks before current roots. If the current artifact itself exceeds the
per-document budget, serve it without caching cross-revision chunks. Eviction may
increase CPU but cannot change output or freshness.

Numeric byte defaults are an activation gate, not a guess in this ADR: the
measurement PR must report retained bytes for the required corpus and choose
limits before any cross-revision reuse is enabled. No implementation may merge
with an unbounded cache in the interim.

### 7. Producer-driven prefetch

After a current `ParseSnapshot` publishes, a document that recently served
semantic tokens may prefetch its current artifact. Prefetch uses the same
revision key and cancellation rules, and a request joins it rather than starting
duplicate work. Prefetch has lower admission priority than parse publication and
foreground request work. It is enabled only after mixed-workload attribution
shows no regression; it is not part of the initial seam.

### 8. Staged delivery and measurement gates

Implementation proceeds as independent PRs:

1. add phase/allocation/retained-byte measurements and dirty-footprint fixtures;
2. introduce the immutable artifact seam around unchanged full computation;
3. single-flight same-revision production and independently shape request views;
4. store fully rebuilt output in persistent chunks with bounded retention;
5. compute incremental candidates in shadow mode and compare to the oracle;
6. enable one proven safety class/language at a time with automatic fallback;
7. add local finalization and structural delta generation separately; and
8. evaluate producer prefetch only after scheduler attribution.

Each behavioral performance PR uses the exact optimized A/B binary contract:
four alternating pairs, six warmups, 30 retained samples, raw evidence, freshness
validation, and controls. Required scenarios include large and extra-large Rust,
injection-heavy Markdown, Unicode/multiline, range, one-thread, two-document,
burst, and cancellation workloads with 1%, 10%, and 50% dirty syntax footprints.
Report CPU, allocations, retained bytes, fallback rate, shadow mismatches,
changed chunks, latest-edit response latency, and accepted responses per second.

## Considered Options

### A. Continue only whole-document local optimizations

This preserves the smallest implementation and remains the fallback path.
However, it cannot make work asymptotically proportional to a small edit and has
already left ordinary large-document typing dominated by current-revision work.

### B. Build immutable revision-scoped artifacts on `ParseSnapshot`

This reuses established ownership, versioning, and cancellation boundaries. It
supports conservative fallback and staged proof without adding a second mutable
document authority. It requires a persistent representation, explicit memory
accounting, and substantial characterization work. **Chosen.**

### C. Adopt a general incremental-computation framework such as Salsa

Such a framework provides dependency tracking and memoization. Kakehashi already
has a Salsa-shaped immutable parse revision, while parser reuse, cooperative
cancellation, LSP baseline history, async bridge I/O, and byte-budgeted eviction
would still require custom ownership. Adopting a framework now adds migration
cost before those boundaries are proven.

### D. Move text and semantic reads into a per-document mailbox actor

An actor can serialize state transitions, but it centralizes mutable ownership
and does not make query walks or finalization cheaper. It also conflicts with the
input/derivation separation in parse-snapshot-architecture and risks head-of-line
blocking between independent readers.

### E. Extend or fork Tree-sitter query execution for incremental match reuse

Query-engine reuse could eventually remove a remaining dominant walk. It is a
high-maintenance dependency fork and does not solve finalization, delta shaping,
retention, or injection identity. Consider it only if profiles after artifact and
chunk reuse still show query execution as the limiting phase.

## Consequences

### Positive

- Full, delta, and range can share one exact current-revision computation.
- Proven edits can eventually reuse work proportional to their semantic impact.
- Immutable publication and full fallback make partial or stale artifacts
  unservable by construction.
- The architecture creates explicit locations for CPU, allocation, fallback,
  mismatch, and retention measurements.

### Negative

- Persistent chunks, invalidation classes, and byte accounting add substantial
  implementation and testing complexity.
- Conservative safety classification means early stages may consume memory
  without producing a speedup.
- Shadow comparison temporarily computes both incremental and full paths and is
  intentionally slower.
- Single-flight ownership and consumer cancellation require careful race testing.

### Neutral

- Whole-document computation remains permanent as the oracle and fallback.
- Existing request result IDs remain per client/session lineage; the artifact is
  shared computation, not a globally accepted LSP baseline.
- Bridge virtual-document lifecycle remains outside this decision and is handled
  separately by issue #897.

## Confirmation

The decision is confirmed incrementally, not by the presence of an artifact type:

- seam and chunk stages flatten byte-for-byte to the current full oracle;
- each enabled safety class has zero shadow mismatches across retained and
  randomized edits, including injection, multiline, Unicode, reopen, reload, and
  parser-install cases;
- freshness tests prove accepted responses represent the latest edit and that
  cancelling one consumer does not cancel another current consumer;
- slot tests rotate keys on a generation change, reject completion from the
  detached producer, and retry the same key after cancellation, timeout, or
  panic;
- retention tests preserve reissued result IDs under the existing bounded LRU,
  enforce the new artifact byte budgets, and cover close/reopen and cancellation
  cleanup;
- at least one large-document small-edit scenario has repeatable proportional
  work and lower current-response latency without regressing primary controls;
  and
- every activation PR retains exact A/B evidence and falls back automatically on
  an unknown or invalidated class.

## More Information

- [Issue #895: improve semantic-token performance](https://github.com/atusy/kakehashi/issues/895)
- [Issue #896: explore revision-scoped incremental semantic-token artifacts](https://github.com/atusy/kakehashi/issues/896)
- [PR #902: attribute shared compute work](https://github.com/atusy/kakehashi/pull/902)
- [PR #903: cancel obsolete document work](https://github.com/atusy/kakehashi/pull/903)
