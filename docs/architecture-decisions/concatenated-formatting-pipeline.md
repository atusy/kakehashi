# Concatenated Formatting Pipeline

> Scoped to `textDocument/formatting` within a single injection region (see the
> virtual document model in language-server-bridge-virtual-document-model).
> `textDocument/rangeFormatting` is **out of scope**: although it resolves
> aggregation under the same `textDocument/formatting` key, it always uses the
> `preferred` strategy and ignores `concatenated` (rationale under *Decision*).
> Per-method strategy selection and the cross-file/edit-filtering rules live in
> language-server-bridge-request-strategies; the `AggregationStrategy` enum and
> fan-in mechanics live in ls-bridge-server-pool-coordination.

## Context

A single injection region may have **multiple downstream language servers**
configured for the same language. For most methods, the `preferred` strategy
(first non-empty response wins) is the right default, and for list-producing
methods (`textDocument/diagnostic`, `references`) the `concatenated` strategy
concatenates the **result lists** from all servers.

Formatting is different. Real-world formatting setups routinely chain several
tools over one document:

- complementary, minimal-edit tools: `isort` (imports), `autoflake` (unused
  removal), `eslint --fix` (lint fixes) — each touches a different span;
- whole-document formatters: `black`, `prettier`, `gofmt` — each returns one
  edit that replaces (nearly) the entire region.

Users want to combine **both kinds** in one region (e.g. `black` then `isort`).
The previously documented rule (ls-bridge-server-pool-coordination) said
formatting *must* use a single server, precisely because naively merging `TextEdit[]` from several
formatters violates the LSP "edits must not overlap" rule — two whole-document
formatters always overlap.

The list-concatenation mechanics used by diagnostics/references do **not**
transfer to formatting: concatenating two formatters' edit lists produces
overlapping edits. So formatting needs its own meaning for "combine multiple
servers".

### Two-level structure

Formatting over a host document has two nesting levels, and overlap-freedom holds
at each level for a **different** reason:

- **Across injection regions — parallel.** A host document resolves to several
  injection regions, each a disjoint span of the host. They are formatted
  concurrently (one task per region) and their resulting edits are concatenated;
  disjointness means the concatenation can never overlap. This is existing
  behavior, owned by request-strategies, and is unchanged by this decision.
- **Within one region — sequential.** When a single region has multiple servers,
  this decision runs them serially over the same text. Here overlap-freedom comes
  from seriality (each server sees the prior server's output), not disjointness.

The asymmetry is deliberate: regions are parallel because they are disjoint;
servers within a region are serial because they are *not* — running them in
parallel would reintroduce the overlapping-edit problem. Everything below
concerns the within-region level only.

### Why not parallel diff-stacking

An earlier idea was to compute each server's edits against the **original**
region text in parallel, then stack non-conflicting diffs like a git merge and
serialize only the conflicting ones. This was rejected because:

- whole-document formatters always overlap, so the non-conflicting fast path
  almost never applies for the mixed case we are targeting;
- stacking original-based diffs requires **re-basing** each later edit's ranges
  after applying earlier ones (offset drift), reintroducing the overlap-math the
  approach was meant to avoid;
- arrival-order stacking makes the result depend on process/network timing —
  **non-deterministic formatting**, which is unacceptable for a formatter;
- conflict resolution by re-request risks **oscillation** (two formatters that
  each undo the other) and needs cycle/fixpoint guards.

## Decision

**Treat `strategy: "concatenated"` on `textDocument/formatting` as an explicit
opt-in to a sequential formatter pipeline driven by `priorities`.**

1. **Explicit switch.** The pipeline activates only when the resolved
   aggregation config for the method sets `strategy = "concatenated"`. With the
   default `preferred` strategy (or no config), formatting keeps the existing
   first-non-empty-wins behavior. There is no implicit activation.

2. **`priorities` is the pipeline definition.** When the pipeline is active,
   `priorities` is both the **membership list** and the **application order**.
   Servers configured for the language but **absent from `priorities` do not run**
   for formatting — `priorities` acts as an allowlist plus order. An active
   `concatenated` strategy with an empty `priorities` is a misconfiguration and
   falls back to `preferred` (with a warning), since order would otherwise be
   undefined.

3. **Sequential application (single pass).** For each server in `priorities`
   order, against the **current** region text:
   1. push the current region text to the downstream server via `didChange`;
   2. send `textDocument/formatting`;
   3. apply the returned edits to the region text (empty edits = already
      formatted = no-op);
   4. proceed to the next server with the updated text.
   Because each server always sees the latest text, edits **cannot overlap**
   across servers — there is nothing to merge. The pipeline runs **one pass**;
   recursion / fixpoint re-formatting is explicitly out of scope.

4. **Region full-replacement output.** After the last server, the pipeline emits
   a **single `TextEdit` that replaces the entire region** with the final text
   (range = whole virtual document, translated to host coordinates via the
   region offset). It does **not** attempt to compute a minimal diff. This keeps
   the LSP output trivially non-overlapping and avoids needing a
   text-edit-composition or diff utility.

5. **Range formatting stays on `preferred`.** Although `textDocument/rangeFormatting`
   shares this aggregation config (it resolves `strategy`/`priorities` under the
   `textDocument/formatting` key), the pipeline applies to **full formatting
   only**. Range formatting always uses `preferred`, even when
   `strategy: "concatenated"` is configured. A sequential pipeline over a
   sub-range would reintroduce the offset drift full formatting avoids — each
   server's edits shift the requested range, forcing per-step range re-mapping
   and clipping the output back to the selection — and that cost is not worth it
   for partial, interactive formatting where chaining is a rare need. Users who
   want multi-formatter chaining trigger full-document formatting instead.

6. **Best-effort on failure (skip-and-continue).** If a server in the pipeline
   fails — LSP error, crash, or per-step timeout — the pipeline **skips it and
   proceeds to the next server with the current accumulated text**. It never
   aborts the pipeline or discards earlier successful steps. The emitted edit
   therefore reflects whatever steps succeeded, degrading to a no-op edit only
   when every server failed or the text is unchanged; it never falls back to
   throwing away formatting already produced. This mirrors the partial-results
   philosophy of ls-bridge-server-pool-coordination (return what succeeded rather
   than fail the whole request). Each step runs under the overall pipeline
   timeout budget, and a failing step is logged so the misbehaving server is
   diagnosable.

The pipeline reuses the existing per-server virtual-document and
position-translation machinery; the new parts are (a) strategy dispatch, (b) the
intermediate `didChange` that feeds each server's output into the next, and
(c) collapsing the final text into one region-replacement edit.

### Keyword overload, made explicit

`strategy: "concatenated"` means **different mechanics per method**:

| Method family | `concatenated` mechanics | Direction |
|---------------|--------------------------|-----------|
| diagnostics, references, code actions | concatenate **result lists** from all servers | parallel fan-in |
| formatting (this decision) | **sequential text pipeline**, each server's output feeds the next | serial |

Same config keyword, deliberately, so users reach for one familiar switch; the
per-method behavior is documented here and in request-strategies.

### Example

```toml
# Format Python injections in Markdown by running black, then isort.
[languages.markdown.bridge.python.aggregation."textDocument/formatting"]
strategy = "concatenated"
priorities = ["black", "isort"]
```

## Considered Options

### A. Sequential pipeline over `priorities`, region full-replacement (chosen)

Deterministic (config order), trivially non-overlapping (one pass, one output
edit), handles the mixed whole-document + complementary case, and matches how
formatter chains (`black` then `isort`) actually work. Cost: fully serial
(latency = sum of round-trips) and requires `didChange` choreography to feed each
server.

### B. Parallel diff-stacking with conflict re-request (rejected)

The git-merge-style approach. Rejected for the reasons in *Why not parallel
diff-stacking*: ineffective for whole-document formatters, non-deterministic on
arrival order, offset-rebasing complexity, and oscillation risk. May be revisited
as a latency optimization only if profiling shows many complementary minimal-edit
formatters dominate and serial latency hurts.

### C. Keep `preferred`-only for formatting (status quo, rejected)

Simple but cannot chain complementary formatters at all — the user must pick one
tool per region. Insufficient for the mixed real-world setups motivating this
decision.

### D. Minimal-diff output instead of full replacement (deferred)

Emitting a minimal `TextEdit` set (via a Myers-style diff of original vs final)
would shrink the edit payload and play nicer with editor undo granularity. It
requires a diff utility the codebase lacks. Deferred until there is evidence the
full-replacement payload causes problems; the output form is internal and can
change without affecting the config surface.

## Consequences

### Positive

- **Chains complementary + whole-document formatters** in one region with a
  single, deterministic config switch.
- **Trivially LSP-compliant output**: one region-replacement edit per region can
  never overlap, satisfying the no-overlapping-edits rule by construction.
- **Deterministic & reproducible**: result depends only on `priorities` order,
  not on response timing.
- **No new diff/edit-composition machinery**: reuses existing virtual-document
  and position-translation code.

### Negative

- **Serial latency**: total time is the sum of per-server round-trips plus the
  intermediate `didChange` processing; a per-pipeline timeout budget and
  per-step cancellation checks are required.
- **Downstream statefulness**: feeding each server requires a `didChange` and
  waiting for it to take effect before re-requesting — more protocol
  choreography than a stateless forward.
- **Coarse output**: full-region replacement enlarges the edit payload and can
  coarsen editor undo granularity until option D is taken.
- **`priorities` semantics overload**: for formatting, `priorities` becomes an
  allowlist+order (servers not listed do not run), unlike `preferred` where it is
  only a tie-break ordering. Documented, but a behavioral nuance.

### Neutral

- Ordering is the user's responsibility; a bad order (e.g. a formatter that
  reverts a previous tool) produces a bad-but-deterministic result, not an error.
- Recursion/fixpoint re-formatting and minimal-diff output are left as future
  options without committing to them.
- **Shared config, asymmetric effect**: `strategy: "concatenated"` set under the
  `textDocument/formatting` key affects full formatting but is silently ignored
  by `rangeFormatting` (which stays on `preferred`). This is a deliberate scope
  cut, not an oversight — keeping range formatting simple — but it means one
  config key drives two methods differently.
- **Silently skipped formatters**: a failing pipeline step is skipped (and
  logged) rather than surfaced to the editor, so a user may not notice that a
  configured formatter did not run. Best-effort robustness is the deliberate
  trade for not failing the whole format request.

## Decision–Implementation Gap

Not yet implemented as of this decision. `textDocument/formatting` currently runs
the `preferred` strategy per region regardless of config, and a misconfigured
`concatenated` formatting configuration only emits a warning rather than running
a pipeline. This record defines the target behavior for full formatting; the
warning path is the placeholder to be replaced.

`textDocument/rangeFormatting` keeps the `preferred` behavior **by design**, not
just pending implementation — it is out of scope for the pipeline (see *Decision*
point 5), so this decision leaves it unchanged.

## Related Decisions

- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md): Per-method bridge strategies, including formatting's edit handling
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md): `AggregationStrategy` enum, fan-out/fan-in, and aggregation timeout rules
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md): How injection regions are represented as virtual documents
