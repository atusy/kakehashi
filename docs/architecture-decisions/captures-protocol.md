# Captures Protocol

**Related Decisions**:
- [node-reference-protocol](node-reference-protocol.md) — Node identity, `NodeInfo`, and the `kakehashi/node/*` accessor family the capture results compose with
- [lazy-node-identity-tracking](lazy-node-identity-tracking.md) — ULID identity tracking reused to make captured nodes addressable across edits

## Context and Problem Statement

node-reference-protocol exposes tree-sitter's `Node` API one node at a time, but offers no way to run a tree-sitter **query** — the declarative, per-language `.scm` assets that encode which structural shapes matter (context headers, folds, textobjects, …). The motivating consumer is a [nvim-treesitter-context](https://github.com/nvim-treesitter/nvim-treesitter-context)-style sticky-context feature: find the `@context` captures relevant to the cursor and render their ranges.

A first iteration shipped `kakehashi/query`: a one-shot method taking a **client-supplied query string** and returning its matches. Reviewing it against the real consumer exposed a mismatch in interaction model:

- Context/fold/textobject features re-request on **every cursor move, scroll, and edit**. Re-sending the same multi-kilobyte query string per request is wasteful, and a string-keyed protocol gives the server nothing stable to cache or diff against.
- The queries in question are not ad-hoc — they are **named, per-language assets** (`queries/<lang>/context.scm`) installed into runtime directories, exactly like the highlights/injections queries kakehashi already loads from its `searchPaths`.
- LSP already has a proven interaction model for "bulk syntax data recomputed per edit": `textDocument/semanticTokens` with its `full` / `full/delta` / `range` triple. Reusing that shape makes the protocol instantly familiar and gives clients a wire-efficient repeat-request path.

## Decision Drivers

* **Live-consumer economy**: repeat requests (cursor move, scroll, edit) must not resend the query or the full unchanged result.
* **Server-owned, dynamically discovered kinds**: the query is named by a free-form `kind` resolved as `queries/<lang>/<kind>.scm` from `searchPaths` — *not* a hard-coded enum — so new kinds appear by dropping files into a search path, and a future method can enumerate available kinds from the directory structure.
* **Match correlation preserved**: `@context` and `@context.end` within one pattern must stay grouped; a flat capture stream loses which `.end` bounds which `@context`.
* **Reuse proven mechanics**: the semanticTokens `full`/`delta`/`range` interaction model, the existing tolerant query compilation (`; inherits:`, per-pattern skip), predicate evaluation, ULID minting, and the prefix/suffix single-edit delta algorithm.
* **Minimal API surface**: one protocol for query execution; `kakehashi/query` is replaced, not kept alongside.

## Decision Outcome

**Chosen approach**: Replace `kakehashi/query` with three methods mirroring the semanticTokens triple, parameterized by a server-resolved query `kind`:

| Method | Input | Output |
|---|---|---|
| `kakehashi/captures/full` | `{ textDocument, kind, injection? }` | `CapturesResult \| null` |
| `kakehashi/captures/full/delta` | `{ textDocument, kind, previousResultId }` | `CapturesResult \| CapturesDelta \| null` |
| `kakehashi/captures/range` | `{ textDocument, kind, range, injection? }` | `CapturesRangeResult \| null` |

### Result shapes

```typescript
type CapturesResult = {
  resultId: string;            // hand back via previousResultId for a delta
  matches: Match[];
  skipped: SkippedPattern[];   // patterns dropped by tolerant compilation
};

type Match = {
  patternIndex: number;        // within the producing language's kind query —
                               // (language, patternIndex) is the unambiguous key
  language: string;            // language of the layer that produced this match
  metadata?: Metadata;         // match-level `#set!` directives
  captures: {
    name: string;              // capture name without '@', e.g. "context"
    node: NodeInfo;            // { id, kind } — trackable via kakehashi/node/*
    range: Range;              // LSP Range (UTF-16), inline to avoid N+1
    metadata?: Metadata;       // capture-scoped `#set! @cap` directives
  }[];
};

type Metadata = { [key: string]: string | true };

type CapturesDelta = {
  resultId: string;
  edits: { start: number; deleteCount: number; data: Match[] }[];
};

type CapturesRangeResult = { matches: Match[]; skipped: SkippedPattern[] };

type SkippedPattern = { language: string; startLine: number; endLine: number; reason: string };
// startLine/endLine are 1-indexed lines in the query file — or in the combined
// query string when `; inherits:` is used (the loader concatenates parents first).
```

`language` is present on every match — including host-only requests — so the
wire shape never branches on the request mode, and it lives **per match** (not
per capture, which would be redundant; not as a per-layer grouping, which would
break the flat-array delta). Because the diff is plain JSON equality over match
objects, adding `language` required no change to the delta algorithm.

**`#set!` metadata** (the Neovim
[`treesitter-directive-set!`](https://neovim.io/doc/user/treesitter.html#treesitter-directive-set!)
convention) follows its two scopes onto the wire: `(#set! key value)` becomes
the match's `metadata`, `(#set! @cap key value)` becomes that capture's —
mirroring Neovim's `metadata[key]` vs. `metadata[capture_id][key]` split, with
the capture *name* standing in for the capture id (the index is meaningless
across the wire). The field is **omitted when a pattern sets nothing**, keeping
pre-metadata wire shapes byte-identical; and because the directives are static
per pattern, the positional delta diff is unaffected — equal JSON still means
an identical match. Values stay the strings written in the query (clients
coerce, as Neovim consumers do); the bare flag form `(#set! key)` surfaces as
`true` rather than Neovim's nil no-op, so a flag is actually observable.
Repeated keys are last-write-wins, matching Neovim's in-order directive
application. Other directives (`#offset!`, `#gsub!`, `#trim!`) compute
runtime-dependent metadata and are **not** implemented — `#set!` is purely
static, so it falls out of tree-sitter's own `property_settings` parsing.

### Kind resolution

`kind` names a query file: the server resolves `queries/<language>/<kind>.scm` across its configured `searchPaths` (first hit wins), honoring `; inherits:` directives and tolerant per-pattern compilation — the identical pipeline used for highlights/bindings/injections. `kind` is validated as `[A-Za-z0-9_-]+`; anything else (path separators, dots) is rejected as `InvalidParams` before touching the filesystem. The fixed `QueryKind` enum remains an internal detail of config-time loading; this protocol deliberately does **not** extend it, so available kinds are defined purely by what files exist — enabling a future `kakehashi/captures/kinds` discovery method with no protocol change.

### The `injection` parameter

`injection` is a plain boolean (default `false`) on `full` and `range`:

- `false` / absent — run the kind query against the **host** tree only (all result nodes minted in layer 0);
- `true` — run it across **every** layer: the host first, then each injection region in document order, recursing into nested injections up to the same depth cap as the cursor-path injection stack (`MAX_INJECTION_DEPTH` injected layers — the convention that matters, because minted ids resolve through that stack's depth indexing). Each layer resolves **its own language's** `queries/<lang>/<kind>.scm`; a layer whose language has no kind file simply contributes nothing. Result nodes are minted in their layer's depth, so they compose with `kakehashi/node/*` under the node-reference-protocol per-layer Scope rule (with the same depth-index caveats lazy-node-identity-tracking documents; ids minted from *overlapping* same-depth regions may re-resolve in the wrong same-depth region or collapse to `null` — issue #350).

Unlike `kakehashi/node`'s `boolean | number` selector, there is no layer *indexing*: captures have no cursor position to anchor a layer stack, so the only meaningful modes are "host only" and "everything". The kind is considered available when **at least one** visited layer's language has the kind file — e.g. a host language without a `context.scm` still yields the embedded layers' contexts; only "no visited language has the file" collapses to `null`.

**Match ordering** is document-order DFS — the host layer's matches in query order, then each injection region by ascending start byte, each region's matches (and its nested regions) before the next sibling region. The order is deterministic, which the positional delta diff requires.

### Delta semantics

`full` responses carry a `resultId`, and the server keeps **one lineage slot per `(uri, kind, injection-mode)`** — host-only and injection lineages live side by side, so two clients driving the same document in different modes never clobber each other's lineage. **Delta requests carry no `injection` parameter: `previousResultId` selects its lineage, and with it the mode**; a client switches modes by issuing a new `full`. A `full/delta` request:

- if `previousResultId` matches a slot, recomputes the current matches under that slot's mode and returns a **single-edit diff** over the matches array — common prefix and suffix are dropped, the middle is replaced (`start` / `deleteCount` are match indices, mirroring the semantic-tokens delta algorithm in `src/analysis/semantic/delta.rs`);
- if the id is stale but **exactly one** mode slot is live, returns a full `CapturesResult` computed under that mode (the LSP "server may always answer a delta with a full" convention, with the mode unambiguous);
- otherwise — no lineage at all (never `full`ed, dropped by `didClose`, server restart), or a stale id while **both** mode slots are live — returns **`null`**. This deliberately deviates from semanticTokens' always-full fallback: with the mode inherited rather than carried, answering would mean *guessing* the client's `injection` intent, and a host-only guess would silently serve an injection client the wrong layer set. Universal `null` ("re-acquire") routes the client back to `full`, which carries the flag.

Lineage inserts are serialized with `didClose`'s document removal through the per-URI edit lock, so a request in flight across a close cannot resurrect lineage for a closed document.

Unlike token deltas, match deltas need no line-count safety guard: each match carries **absolute** ranges, so equal JSON means an identical match regardless of what changed elsewhere. Note the diff is positional, not identity-based — and intentionally so: a surviving node whose range shifted *must* appear in the edit (the client needs the new range), which `(id, range)` JSON equality gives for free.

### Null vs. error semantics

* `null` — the document is unresolvable (unknown URI, not parsed, no host language), an internally superseded/closed walk became obsolete, **or the kind has no query file for this language**. Internal freshness control uses the protocol's re-sync signal rather than JSON-RPC cancellation; `RequestCancelled` is reserved for an explicit client `$/cancelRequest`. A missing `context.scm` for some language is the expected common case, not an error: it means "no such captures here". A query file that exists but compiles to nothing degrades to `null` with a server-side warning when it is the only candidate — it is a configuration/asset problem, mirroring how broken highlight queries degrade; under `injection: true`, when *other* languages still yield a result, the broken language contributes no matches but its dropped patterns are reported in `skipped`, so the asset stays debuggable from the client.
* JSON-RPC `InvalidParams` — a malformed `kind` (fails the character whitelist). This differs from the replaced `kakehashi/query`, where a malformed query *string* was the client's bug; here the query content is a server asset, so only the kind *name* remains the client's responsibility.

## Considered Options

### A. Keep `kakehashi/query` (client-supplied string) alongside `captures/*`

Coexistence preserves an ad-hoc escape hatch (try a query without installing a file), but leaves two overlapping execution surfaces to document, test, and keep semantically aligned. Rejected: the live consumers all use named assets; an ad-hoc need can later be served by a `queryString`-bearing variant or a scratch kind without re-introducing a second protocol. Replacement also honors the project's minimal-API-surface driver.

### B. Pure semanticTokens encoding (flat capture array)

Maximum familiarity and the smallest wire format, but flattening loses intra-match correlation (`@context` ↔ `@context.end`), which the motivating consumer requires. Rejected; the match-grouped shape is kept from the first iteration, and the delta operates on whole matches instead of token integers.

### C. Identity-based delta (ULID add/remove sets)

Diff as "these node ids appeared / disappeared". Superficially attractive since ids are already edit-stable, but a surviving id whose range moved would need a third "changed" set, and clients would reconstruct array order themselves. The positional single-edit diff reuses a proven algorithm, keeps client-side application trivial (splice), and treats range changes correctly by construction. Rejected.

### D. Extending the `QueryKind` enum with `Context` etc.

Matches existing config-time loading, but every new kind becomes a code change, contradicting the requirement that kinds be discoverable from the searchPaths directory structure. Rejected; request-time resolution goes straight to the file system through the existing loader.

### E. Per-`(uri, kind)` server cache of compiled queries

Deferred, not rejected: v1 reloads and recompiles the kind query per request. The files are small (a `context.scm` is ~1 KB) and execution over the tree dominates; a compiled-query cache keyed by `(language, kind)` with config-reload invalidation is a contained optimization if profiling warrants it.

### F. Richer `injection` selector (`boolean | number`, as on `kakehashi/node`)

`kakehashi/node` indexes a layer *stack at a cursor position*; captures run over the whole document, where injection regions are siblings, not a stack — an integer index has nothing well-defined to select. The two meaningful modes are "host only" and "all layers", which a boolean expresses exactly. Rejected as YAGNI; a future filter (e.g. by language name) would be a new parameter, not an integer.

### G. Per-capture `language`, or matches grouped by layer

Per-capture is redundant (all captures in a match come from one layer). Grouping (`layers: [{language, matches}]`) preserves structure but abandons the flat matches array the positional delta diffs over. Rejected; `language` per match keeps the delta algorithm untouched — the diff is JSON equality, so schema additions are free.

### H. Delta fallback to host-only when the lineage is missing

Answering a lineage-less delta with a host-only full (the semanticTokens convention of "always may answer full") would silently serve the wrong layer set to a client that had requested `injection: true`. Rejected in favor of `null` — the protocol's universal "re-acquire" signal — because only a new `full` carries the client's intended mode. See Delta semantics.

### I. A `matchLimit` result cap

The first iteration carried an optional per-request cap (plus a server default). Rejected on review: truncation is **silent** — a client cannot tell a complete result from a clipped one, which for the motivating consumer means silently wrong contexts — and a truncated `full` poisons the delta lineage (edits computed over a clipped array, worse if `full` and `delta` use different limits). The scoping tool for "too much data" is `range`; semanticTokens, the protocol this mirrors, has no limit parameter either. Result size is bounded in practice by the query asset the server owns, not by hostile client input.

## Consequences

### Positive

* The sticky-context loop becomes wire-efficient: unchanged documents answer with an empty-edits delta; small edits ship only the changed matches.
* Query assets live where the ecosystem already puts them (`queries/<lang>/<kind>.scm` in runtime paths); nvim-treesitter-context's own query files work unmodified once on a search path.
* Kinds are open-ended without protocol or code changes; folds, textobjects, or custom plugin kinds ride the same three methods.
* Captured nodes remain trackable `NodeInfo`s, composing with the entire `kakehashi/node/*` family.
* `injection: true` makes the cross-language sticky-context case (Markdown heading **and** the Python function inside the block) a single request, with each layer's own ecosystem query assets.

### Negative

* Ad-hoc query execution (arbitrary string, no file) is no longer possible over the wire; experimentation requires writing a file into a search path.
* Clients cannot ship inline query variations per request; behavior changes require editing the asset.
* Each request re-reads and recompiles the kind queries (see Option E) — acceptable now, a known lever later.
* `injection: true` re-parses every injection region per request (deltas included — they recompute under the stored mode). The semantic-tokens pipeline pays the same cost per request; sharing or caching layer trees across both is a future optimization, not a v1 concern.

### Neutral

* `resultId` state is per `(uri, kind)` and dropped on `didClose`; a delta against a stale id falls back to a full result under the stored mode, and a delta with no lineage at all returns `null` (see Delta semantics), so the client's only re-sync rule is "on null, call `full` again".
* Adding `language` to the match shape invalidates pre-upgrade lineages: the first delta after a server upgrade diffs against differently-shaped matches and degrades to a full response — self-healing, no client action needed.
* Predicate evaluation reuses the highlighting evaluator (same operators, same permissive unknown-predicate handling) but gates the **whole match**, mirroring Neovim's `iter_matches` and tree-sitter's own built-in-predicate handling: one failing general predicate discards the match and its captures entirely. Highlighting keeps per-capture filtering — there a guard capture should not strip its siblings' colors; here a partial match would leak `#set!` metadata onto matches Neovim rejects.
