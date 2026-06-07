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
methods (`textDocument/diagnostic`, `textDocument/references`) the `concatenated` strategy
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
  behavior, owned by language-server-bridge-request-strategies, and is unchanged
  by this decision.
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

Treat `strategy: "concatenated"` on `textDocument/formatting` as an explicit
opt-in to a sequential formatter pipeline driven by `priorities`.

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
   1. make the current accumulated text available to the server — the scratch
      document's `didOpen` (point 7) carries it; a `didChange` is only needed when
      a single scratch document is reused across steps rather than opened fresh per
      server;
   2. ask that server to format the whole region. The **pipeline** prefers
      `textDocument/formatting`; the fallback keys on **capability**, not on the
      response: only when the server does not advertise `documentFormattingProvider`
      does the pipeline
      **fall back to `textDocument/rangeFormatting` over the entire region** so
      range-only servers still participate — the same whole-region equivalence the
      existing `textDocument/rangeFormatting` handler relies on for covering
      requests. (The current full-formatting path does not itself fall back; this
      is target pipeline behavior.) Crucially, a `null` response from a *capable*
      server is the LSP "no changes / already formatted" signal and is
      **authoritative** — it must **not** trigger the fallback. The implementation
      must therefore distinguish "no capability" from "no changes" rather than
      collapsing both to one no-result value: reserve the no-result path for a
      missing provider, and treat a successful `null` as an empty edit list
      (`[]` / `Ok(Some([]))`). A genuine error is treated as a
      **failed step** — handled by point 6 (skip-and-continue), not surfaced to
      the editor;
   3. apply the returned edits to the region text (empty edits = already
      formatted = no-op);
   4. proceed to the next server with the updated text.
   Because each server always sees the latest text, edits **cannot overlap**
   across servers — there is nothing to merge. The pipeline runs **one pass**;
   recursion / fixpoint re-formatting is explicitly out of scope.

4. **Region full-replacement output.** After the last server, the pipeline emits
   a **single `TextEdit` that replaces the entire region** with the final text
   (range = whole virtual document, translated to host coordinates via the region
   offset). It does **not** attempt to compute a minimal diff. This keeps the LSP
   output trivially non-overlapping and avoids needing a text-edit-composition or
   diff utility. Because the replacement spans multiple lines, the host
   translation
   **must re-apply the region's host prefix/indentation to every output line** —
   not just the first line's offset. The virtual `newText` starts
   at column 0 of the embedded language, so emitting it verbatim would strip those
   prefixes and corrupt blockquoted/indented injections — the single-edit
   limitation noted in `src/lsp/bridge/text_document/formatting.rs` ("multi-line
   edits drop host indentation"), which the pipeline's full-region output must
   resolve. An injection region's host prefix is **uniform** — the same `> ` or
   indentation on each line — so the pipeline applies that one prefix, captured
   once for the region, to all lines of the formatted output. This needs no
   line-by-line diff or alignment (consistent with emitting one full-region
   replacement rather than a minimal diff), and a formatter freely adding or
   removing lines is fine because every output line receives the same prefix.
   On **empty** output lines the prefix's trailing whitespace is trimmed (emit
   `>` not `> `, and leave a space-only prefix off entirely) so the pipeline
   does not introduce trailing-whitespace violations.

5. **Range formatting stays on `preferred`.** Although `textDocument/rangeFormatting`
   shares this aggregation config (it resolves `strategy`/`priorities` under the
   `textDocument/formatting` key), the pipeline applies to
   **full formatting only**. Range formatting always uses `preferred`, even when
   `strategy: "concatenated"` is configured. (Note this is distinct from the
   whole-region range *fallback* inside step 3.2: that shim lets a server lacking
   `textDocument/formatting` still participate in **full** formatting; it is not a
   user-issued `textDocument/rangeFormatting` request, which is what stays on
   `preferred`.) A sequential pipeline over a sub-range would reintroduce the
   offset drift full formatting avoids — each
   server's edits shift the requested range, forcing per-step range re-mapping
   and clipping the output back to the selection — and that cost is not worth it
   for partial, interactive formatting where chaining is a rare need. Users who
   want multi-formatter chaining trigger full-document formatting instead.

6. **Best-effort on failure (skip-and-continue).** If a server in the pipeline
   fails — LSP error, crash, or per-step timeout — the pipeline
   **skips it and proceeds to the next server with the current accumulated text**. It never
   aborts the pipeline or discards earlier successful steps. The emitted edit
   therefore reflects whatever steps succeeded, degrading to a no-op edit only
   when every server failed or the text is unchanged; it never falls back to
   throwing away formatting already produced. This mirrors the partial-results
   philosophy of ls-bridge-server-pool-coordination (return what succeeded rather
   than fail the whole request). Each step's deadline is the **remaining** share
   of the overall pipeline budget (`overall − elapsed`), not a fixed per-step
   timeout, so a slow early step cannot let the cumulative latency overrun the
   client's request timeout — it just exhausts the budget and the rest are
   skipped. Below a small floor (e.g. < 50 ms remaining) the pipeline skips the
   rest outright rather than issuing requests almost certain to time out. A
   failing step is logged so the misbehaving server is diagnosable.

7. **Isolate speculative state in a scratch document (state-consistency invariant).**
   The intermediate `didChange` notifications (step 3.1) push
   *speculative* formatted text — text the editor has **not** applied. The
   editor's truth only changes when it applies the final emitted edit (or stays at
   the original on a no-op / complete failure / cancellation). The **invariant**
   is that no downstream document the rest of the bridge relies on is ever left
   holding speculative text. The **recommended** mechanism is to run the pipeline
   against a **throwaway scratch document** (a unique virtual URI, `didOpen`/
   `didClose` per run) so the canonical virtual document is never speculatively
   mutated at all. This is preferred over mutating the shared document and
   reconciling afterward with a corrective `didChange`, because the shared-mutation
   approach has two hazards under tower-lsp's concurrent request processing:
   - **Concurrent reads see speculative state**: a `hover`/`completion`/diagnostic
     request the downstream server handles *during* the pipeline would evaluate
     against unapplied text — flashes and wrong results.
   - **Reconciliation can fail when it matters most**: if a step times out or the
     server is stuck (point 6), the corrective `didChange` may itself queue behind
     the stuck request or time out, leaving the shared document **permanently**
     desynced. A scratch document needs no corrective step — it is simply
     discarded — so failure can never desync the canonical document.

   The scratch URI is **not** an arbitrary path: downstream servers resolve
   project config (`.prettierrc`, `pyproject.toml`, …) and select a parser from
   the URI's directory and extension. So the scratch URI must keep the canonical
   virtual document's directory and file extension (not a random or
   different-scheme URI), or config discovery and language detection break (naming
   requirements and an example follow inline). The flip side: a real-looking
   scratch
   URI risks the downstream server indexing it as a workspace file (duplicate
   symbols, namespace collisions, stray diagnostics). The bridge therefore
   **must keep the scratch document ephemeral** (`didOpen`/`didClose` bracketing
   the single pipeline run) and
   **discard any server-initiated notifications targeting scratch URIs**
   (notably `textDocument/publishDiagnostics`) so they never reach the editor. The
   scratch URI should also carry a
   **standardized, ignorable marker** in its name (e.g. a `.kakehashi-scratch` infix as in
   `…/file.kakehashi-scratch.py`) while keeping the directory and extension, so
   that anything which does observe paths — file watchers, build tools, test
   runners, hot-reloaders — can recognize and ignore it. (Scratch documents are
   virtual LSP documents whose content is delivered via `didOpen`; a recognizable
   name bounds the blast radius if a server resolves the path on disk.)

The pipeline reuses the existing per-server virtual-document and
position-translation machinery; the new parts are (a) strategy dispatch, (b) the
intermediate `didChange` that feeds each server's output into the next,
(c) collapsing the final text into one region-replacement edit, and (d) the
scratch-document lifecycle that isolates the pipeline's speculative state.

### Keyword overload, made explicit

`strategy: "concatenated"` means **different mechanics per method**:

| Method family | `concatenated` mechanics | Direction |
|---------------|--------------------------|-----------|
| diagnostics, references | concatenate **result lists** from all servers | parallel fan-in |
| formatting (this decision) | **sequential text pipeline**, each server's output feeds the next | serial |

Same config keyword, deliberately, so users reach for one familiar switch; the
per-method behavior is documented here and in
language-server-bridge-request-strategies.

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
  intermediate `didChange` processing; a per-pipeline timeout budget is required.
  Cancellation must be polled **concurrently during** each in-flight step (e.g.
  `tokio::select!` on the request future and the cancel signal), not only between
  steps, so a hung formatter can be aborted immediately rather than blocking the
  pipeline until it returns. On abort the bridge should also send
  `$/cancelRequest` to the active downstream server so it stops the discarded
  formatting task and frees its resources.
- **Downstream statefulness**: feeding each server requires a `didChange` and
  waiting for it to take effect before re-requesting — more protocol
  choreography than a stateless forward.
- **Coarse output**: full-region replacement enlarges the edit payload, can
  coarsen editor undo granularity, and may disrupt the user's cursor position,
  active code folds, or bookmarks within the region until option D is taken.
- **Scratch-document overhead**: isolating speculative state (Decision point 7)
  costs a `didOpen`/`didClose` of a throwaway document per pipeline run, plus the
  per-step `didChange`s — extra protocol traffic to keep the canonical document
  untouched.
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
  by `textDocument/rangeFormatting` (which stays on `preferred`). This is a deliberate scope
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

The current code also collapses a `null` formatting result to "no result", so a
capable server's `null` ("already formatted") is today indistinguishable from a
missing provider and falls through to lower-priority servers. The pipeline's
capability-based fallback (Decision point 3.2) depends on telling these apart, so
that collapse is part of the gap to close.

`textDocument/rangeFormatting` keeps the `preferred` behavior **by design**, not
just pending implementation — it is out of scope for the pipeline (see *Decision*
point 5), so this decision leaves it unchanged.

## Related Decisions

- [language-server-bridge-request-strategies](language-server-bridge-request-strategies.md): Per-method bridge strategies, including formatting's edit handling
- [ls-bridge-server-pool-coordination](ls-bridge-server-pool-coordination.md): `AggregationStrategy` enum, fan-out/fan-in, and aggregation timeout rules
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md): How injection regions are represented as virtual documents
