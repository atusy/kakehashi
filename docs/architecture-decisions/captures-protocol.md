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
| `kakehashi/captures/full` | `{ textDocument, kind }` | `CapturesResult \| null` |
| `kakehashi/captures/full/delta` | `{ textDocument, kind, previousResultId }` | `CapturesResult \| CapturesDelta \| null` |
| `kakehashi/captures/range` | `{ textDocument, kind, range }` | `CapturesRangeResult \| null` |

### Result shapes

```typescript
type CapturesResult = {
  resultId: string;            // hand back via previousResultId for a delta
  matches: Match[];
  skipped: SkippedPattern[];   // patterns dropped by tolerant compilation
};

type Match = {
  patternIndex: number;
  captures: {
    name: string;              // capture name without '@', e.g. "context"
    node: NodeInfo;            // { id, kind } — trackable via kakehashi/node/*
    range: Range;              // LSP Range (UTF-16), inline to avoid N+1
  }[];
};

type CapturesDelta = {
  resultId: string;
  edits: { start: number; deleteCount: number; data: Match[] }[];
};

type CapturesRangeResult = { matches: Match[]; skipped: SkippedPattern[] };

type SkippedPattern = { startLine: number; endLine: number; reason: string };
// startLine/endLine are 1-indexed lines in the query file — or in the combined
// query string when `; inherits:` is used (the loader concatenates parents first).
```

### Kind resolution

`kind` names a query file: the server resolves `queries/<language>/<kind>.scm` across its configured `searchPaths` (first hit wins), honoring `; inherits:` directives and tolerant per-pattern compilation — the identical pipeline used for highlights/locals/injections. `kind` is validated as `[A-Za-z0-9_-]+`; anything else (path separators, dots) is rejected as `InvalidParams` before touching the filesystem. The fixed `QueryKind` enum remains an internal detail of config-time loading; this protocol deliberately does **not** extend it, so available kinds are defined purely by what files exist — enabling a future `kakehashi/captures/kinds` discovery method with no protocol change.

### Delta semantics

`full` responses carry a `resultId` scoped to `(uri, kind)`. A `full/delta` request recomputes the current matches and:

- if `previousResultId` matches the stored result, returns a **single-edit diff** over the matches array — common prefix and suffix are dropped, the middle is replaced (`start` / `deleteCount` are match indices, mirroring the semantic-tokens delta algorithm in `src/analysis/semantic/delta.rs`);
- otherwise returns a full `CapturesResult` (the standard LSP fallback: a server may always answer a delta request with a full result).

Unlike token deltas, match deltas need no line-count safety guard: each match carries **absolute** ranges, so equal JSON means an identical match regardless of what changed elsewhere. Note the diff is positional, not identity-based — and intentionally so: a surviving node whose range shifted *must* appear in the edit (the client needs the new range), which `(id, range)` JSON equality gives for free.

### Null vs. error semantics

* `null` — the document is unresolvable (unknown URI, not parsed, no host language) **or the kind has no query file for this language**. A missing `context.scm` for some language is the expected common case, not an error: it means "no such captures here". A query file that exists but compiles to nothing also degrades to `null` with a server-side warning — it is a configuration/asset problem, mirroring how broken highlight queries degrade.
* JSON-RPC `InvalidParams` — a malformed `kind` (fails the character whitelist). This differs from the replaced `kakehashi/query`, where a malformed query *string* was the client's bug; here the query content is a server asset, so only the kind *name* remains the client's responsibility.

### Scope: host layer only (v1)

Queries compile against one grammar, so v1 resolves and runs the kind query against the **host** tree only, and mints all result nodes in layer 0. Injection-aware capture collection (running each layer's own `<kind>.scm`, as the semantic-tokens collector does for highlights) is the natural follow-up and the reserved evolution path — deferred exactly as node-reference-protocol deferred injection (PR-1 → PR-4).

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

### F. A `matchLimit` result cap

The first iteration carried an optional per-request cap (plus a server default). Rejected on review: truncation is **silent** — a client cannot tell a complete result from a clipped one, which for the motivating consumer means silently wrong contexts — and a truncated `full` poisons the delta lineage (edits computed over a clipped array, worse if `full` and `delta` use different limits). The scoping tool for "too much data" is `range`; semanticTokens, the protocol this mirrors, has no limit parameter either. Result size is bounded in practice by the query asset the server owns, not by hostile client input.

## Consequences

### Positive

* The sticky-context loop becomes wire-efficient: unchanged documents answer with an empty-edits delta; small edits ship only the changed matches.
* Query assets live where the ecosystem already puts them (`queries/<lang>/<kind>.scm` in runtime paths); nvim-treesitter-context's own query files work unmodified once on a search path.
* Kinds are open-ended without protocol or code changes; folds, textobjects, or custom plugin kinds ride the same three methods.
* Captured nodes remain trackable `NodeInfo`s, composing with the entire `kakehashi/node/*` family.

### Negative

* Ad-hoc query execution (arbitrary string, no file) is no longer possible over the wire; experimentation requires writing a file into a search path.
* Clients cannot ship inline query variations per request; behavior changes require editing the asset.
* Each request re-reads and recompiles the kind query (see Option E) — acceptable now, a known lever later.

### Neutral

* `resultId` state is per `(uri, kind)` and dropped on `didClose`; a delta against an unknown id falls back to a full result, so clients need no special re-sync logic.
* Predicate evaluation and tolerant compilation inherit the existing semantics (including permissive unknown-predicate handling), keeping capture results consistent with semantic-token highlighting.
