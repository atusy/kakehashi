# Packed Captures Protocol

**Related Decisions**:
- [captures-protocol](captures-protocol.md) — the human-readable `kakehashi/captures/*` triple this method mirrors and shares compute with; defines `kind` resolution, the `injection` mode, `#set!` metadata scoping, and the match-positional delta this reuses
- [node-reference-protocol](node-reference-protocol.md) — `NodeInfo` identity, deferred here (node columns are a forward-compatible addition)
- [lazy-node-identity-tracking](lazy-node-identity-tracking.md) — ULID minting whose per-capture cost this method avoids by omitting node identity

## Context and Problem Statement

`kakehashi/captures/*` (captures-protocol) returns a match-nested, self-describing JSON shape: an array of matches, each with a `captures` array whose entries repeat `name`, `node: {id, kind}`, and `range: {start: {line, character}, end: {…}}`. That shape is excellent for **sparse, human-readable** kinds — a `context.scm` yields a handful of matches, and a developer can read the payload directly.

It is pathological for **dense** kinds. A `highlights` kind emits **one capture per token**. Each capture serializes to roughly **165 B** of the verbose shape, broken down as:

| Element | ~bytes/capture | Nature |
|---|---|---|
| `node: {id, kind}` (26-char ULID + wrapper) | ~60 | payload **and** compute (mint per token) + identity-tracker pollution |
| `range: {start: {line, character}, end: {…}}` | ~48 | structural key overhead |
| `name` string, repeated | ~15 | dictionary-compressible |
| JSON keys (`name`/`node`/`range`/`start`/`end`/…) | remainder | column-compressible |

At ~165 B/capture, a document with a few thousand tokens crosses into the hundreds of kilobytes — a `full` over `docs/language-features.md`'s own highlights runs to roughly **600 KB**. Every keystroke that misses the delta cache pays that again.

Two consumers with **opposite** optimal encodings are being served by one method:

- a **human/debug/sparse** consumer wants legible, self-describing JSON;
- a **machine/dense** consumer (bulk token rendering, e.g. a highlighter driving off raw query captures rather than the fixed-legend `textDocument/semanticTokens` sweep-line result) wants the smallest wire it can splice quickly, and never addresses a capture node across edits.

Trying to serve both from one method forces either a lossy compromise or an `encoding` request parameter that makes the response type a request-discriminated union — new axes on the delta lineage, a branching client, a union to document. captures-protocol §"Considered Options B" already rejected a flat token array *for the shared method* because it loses match correlation (`@context ↔ @context.end`).

**The `600 KB` pain is `full`/`range` only** — deltas already ship just the diff.

## Decision Drivers

* **Keep the human-readable surface untouched.** The verbose `kakehashi/captures/*` triple stays exactly as captures-protocol specifies; no compromise on legibility, no back-compat negotiation on it.
* **A no-compromise machine format.** A new method, being new, can adopt the ideal dense encoding as its *only* shape — aggressive defaults with no legacy to preserve.
* **Preserve match correlation.** grouping (`@context ↔ @context.end`) is required by the motivating consumers and must survive the flattening (confirmed mandatory).
* **Forward-compatible.** Node identity is deferred now; adding it later must be a purely additive change, not a wire break.
* **Reuse proven mechanics.** Share `compute_captures` (kind resolution, injection walk, predicate gating) verbatim; reuse the match-positional prefix/suffix delta; reuse the per-`(uri, kind, mode)` lineage discipline.

## Decision Outcome

**Chosen approach**: grow a **separate method family** that emits a **packed encoding with a per-result legend**, alongside — not replacing — the verbose triple.

The packing combines three techniques: a **columnar layout** (transpose the array-of-objects into parallel arrays, one per field, so each JSON key is written once), **legend interning** (repeated strings become small integer indices into a per-result dictionary), and **integer-packed ranges** (a nested `{start, end}` becomes four flat ints). Together:

```jsonc
// verbose: array-of-objects — keys + strings repeat every capture
[ { "name": "keyword", "range": {…} }, { "name": "keyword", "range": {…} }, … ]
// packed: columnar arrays + match grouping + interned names + flat int ranges
{ "name": [0, 0, …], "matchId": [0, 0, …], "range": [10,4,10,11, 12,0,12,6, …] }  // 0 → legend.names[0] == "keyword"
```

The method family:

| Method | Input | Output |
|---|---|---|
| `kakehashi/captures/packed/full` | `{ textDocument, kind, injection?, nodes?: false }` | `PackedResult \| null` |
| `kakehashi/captures/packed/full/delta` | `{ textDocument, kind, previousResultId, nodes?: false }` | `PackedResult \| PackedDelta \| null` |
| `kakehashi/captures/packed/range` | `{ textDocument, kind, range, injection?, nodes?: false }` | `PackedRangeResult \| null` |

`nodes` is a reserved request flag in this ADR, but only `false`/omitted is
accepted in the initial version. A future `nodes: true` extension is described
below and lives in a separate lineage slot.

The namespace lives **under `captures/`** to signal that it shares all semantics (kind resolution, `injection` mode, `#set!` metadata, null-vs-error) with the verbose triple — only the wire encoding differs. Only `compute_captures`'s **wire shaping** and the **lineage slot** are new; the compute pipeline is shared verbatim.

### Why "packed"

The name should tell a client author **why to reach for the method** (a compact, machine-oriented wire that must be unpacked), not **how it is laid out internally**. It also reads as the natural antonym of the verbose default it sits beside. The layout techniques (columnar / legend / int-packing) are described in prose above; the *method* is named for its purpose. Alternatives considered are recorded in Considered Option G.

### Why a separate method, not an `encoding` parameter

- **Monomorphic response type.** One method → one shape; no request-discriminated union, no client branch on a param, no fourth axis (`encoding`) on the delta lineage (already `(uri, kind, injection-mode)`).
- **Per-method capability advertisement.** The machine method is advertised/gated independently.
- **Freedom to set ideal defaults.** Being new, it carries the dense encoding as its default with no legacy call to keep byte-identical — the whole reason a compromise param is avoidable.

### Result shapes

```typescript
type PackedResult = {
  resultId: string;
  legend: Legend;              // query-dependent, shipped here, stable across the lineage
  matches: MatchColumns;       // one entry per match
  captures: CaptureColumns;    // one entry per capture, grouped by match in order
  metadata?: MetadataTable;    // sparse; omitted entirely when no pattern set any #set!
  skipped: SkippedPattern[];   // as captures-protocol
};

// Named columns — deliberately an OPEN object of columns, NOT a fixed-width
// interleaved integer tuple. New columns (e.g. node `kind`/`id`) are then a
// purely additive change; a fixed tuple would make any addition a wire break.
type Legend = {
  names: string[];             // capture names without '@', e.g. "keyword"
  languages: string[];         // producing-layer languages
  // `kinds: string[]` appears only once node columns are added (see below)
};

type MatchColumns = {
  patternIndex: number[];      // parallel arrays, index = match index
  language: number[];          // index into legend.languages
  [column: string]: unknown[];  // OPEN: future columns are additive
};

type CaptureColumns = {
  name: number[];              // index into legend.names
  matchId: number[];           // index into MatchColumns — restores grouping, 1 int/capture
  range: number[];             // FLAT, 4 ints/capture: [startLine, startChar, endLine, endChar]
                               // absolute (UTF-16), NOT delta-encoded (see Delta semantics)
  [column: string]: unknown[];  // OPEN: future columns are additive
};

type MetadataTable = {
  // Sparse: only matches/captures that carry #set! appear. Absent field => none.
  // JSON object keys are strings on the wire; numeric indices are written as
  // decimal string property names such as "0" and "42".
  match?:   { [matchIndex: number]:   Metadata };
  capture?: { [captureIndex: number]: Metadata };
};
type Metadata = { [key: string]: string | true };  // as captures-protocol

type MatchBlock = {
  matches: MatchColumns;
  captures: CaptureColumns;
  metadata?: MetadataTable;
  // `matchId` values are rebased to 0-based within the block; the client
  // offsets them by the edit's `start` on splice. All legend indices reference
  // the result-wide legend.
};

type PackedDelta = {
  resultId: string;
  edits: { start: number; deleteCount: number; data: MatchBlock }[];
};

type PackedRangeResult = { legend: Legend; matches: MatchColumns; captures: CaptureColumns; metadata?: MetadataTable; skipped: SkippedPattern[] };
```

**Capture ordering invariant**: captures are emitted in match order — all of match *k*'s captures precede match *k+1*'s (document-order DFS across layers, as captures-protocol §"Match ordering" already guarantees). So a match-range splice maps to a **contiguous** capture-range splice, which the delta relies on.

### Node identity is deferred (and easy to add)

This version emits **no node identity**. Machine consumers render tokens; they do not call `kakehashi/node/*` per token, so the ULID is pure cost (the largest per-capture element, plus mint CPU, plus identity-tracker pollution — see Context).

Adding it later is **purely additive** by construction:

1. **Named columns, not a fixed tuple.** A future `nodes: true` grows a `legend.kinds` section, a `captures.kind: number[]` column, and a `captures.id: string[]` column. Existing clients (that never pass `nodes`) see a byte-identical response; clients ignore unknown columns.
2. **Request-gated default-off.** `nodes` defaults to `false`, so the addition changes no existing call.
3. **Lineage key reserves the axis now.** The lineage key is `(uri, kind, injection-mode, nodes)` from day one, with `nodes` always `false` in this version. When `nodes: true` ships, its lineage lives in a distinct slot and never clobbers a `nodes: false` client's lineage (mirroring how `injection-mode` already splits slots).

### Delta semantics

Reused from captures-protocol at **match granularity**, unchanged in spirit:

- Positions stay **absolute**, so "equal per-match JSON = identical match" still holds and the prefix/suffix single-edit diff applies over the internal `Vec<Match>` exactly as today. Going relative/delta-encoded (true semanticTokens style) would break that property — an upstream shift changes every downstream relative value — and force the semanticTokens integer-array delta instead. Rejected; positions are an absolute 4-int column.
- An edit is `{ start, deleteCount, data }` where `data` is a `MatchBlock`. The client splices `matches` columns `[start, start+deleteCount)` with the block's, deletes the **contiguous** capture slice belonging to those matches (the ordering invariant guarantees contiguity), inserts the block's capture columns, and offsets the block's rebased `matchId` by `start`.
- If the inserted match count differs from `deleteCount`, the client also rebases every surviving capture whose existing `matchId >= start + deleteCount` by `insertedMatchCount - deleteCount`; otherwise those captures point at the wrong trailing match after the splice.
- Metadata keys are absolute indices and shift with their columns. The client removes `metadata.match` entries in `[start, start+deleteCount)`, shifts later match keys by `insertedMatchCount - deleteCount`, offsets incoming block match metadata keys by `start`, and applies the same remove/shift/offset rule to `metadata.capture` using the deleted capture slice start/count and the inserted capture count.
- `resultId` lineage, stale-id fallback, both-modes-live → `null`, and `didClose` serialization are identical to captures-protocol §"Delta semantics" — same code, different serializer.

The `legend` is shipped in `full`/`range` and referenced (not re-sent) by deltas; it is stable across a lineage because a config reload that could change it invalidates the lineage (→ new `full`). This is precisely why the encoding must **carry its own legend** rather than assume a fixed one like `textDocument/semanticTokens`: capture legends are query-dependent.

### Null vs. error semantics

Identical to captures-protocol §"Null vs. error semantics": `null` for an unresolvable document / a kind with no query file for any visited language / an asset compiling to nothing / a lineage-less delta; `InvalidParams` for a malformed `kind`.

## Considered Options

### A. One method with an `encoding: "verbose" | "packed"` parameter

Shares one lineage and one handler. Rejected: makes the response a request-discriminated union (client branches on the param it sent), adds a fourth axis to the delta lineage, and forces the verbose method to keep its shape byte-identical forever — losing the "new method → ideal default" freedom that motivates this work. A separate method costs only a thin serializer + a lineage slot because `compute_captures` is shared regardless.

### B. Flat semanticTokens-style integer array (no `matchId`)

Maximum compression, but drops match grouping — the same objection captures-protocol §B raised. Rejected: grouping is mandatory here. The `matchId` column buys grouping back for **1 int/capture**, so the packed layout with `matchId` strictly dominates a pure token array at equal density; the pure array is merely the degenerate case of this encoding with `matchId` (and node columns) removed.

### C. Fixed-width interleaved integer tuple (like semanticTokens' 5-int stride)

Slightly smaller than named columns (no column keys), but any future field (node `kind`/`id`) is a stride change = wire break for every client. Rejected in favor of named columns (an open object), whose only cost is a handful of one-time column-name keys and whose payoff is free additive evolution — the explicit forward-compat requirement.

### D. Relative/delta-encoded positions (true semanticTokens encoding)

Would shave the range column further, but breaks the absolute-position invariant the match-positional delta depends on, dragging the whole delta design onto the integer-array algorithm. Rejected; the win does not pay for the coupling. Positions are absolute 4-int.

### E. Replace the verbose triple entirely

Rejected: the human-readable surface has a real consumer (debugging, ad-hoc inspection, sparse kinds where legibility beats bytes). The two shapes have opposite optima; keeping both is the point.

### F. Ship node columns now

Rejected as YAGNI: the confirmed near-term consumer renders tokens and does not address nodes across edits, so the ULID is pure cost. The design keeps the addition additive (named columns + request gate + reserved lineage axis), so deferring loses nothing.

### G. Method naming — `columnar` / `tokens` / `dense` / `compact`

The method needs a one-word modifier distinguishing it from the verbose triple. Candidates weighed:

| Name | Names | Verdict |
|---|---|---|
| **columnar** | the SoA *layout* | Precise but domain-borrowed (DB/analytics) for an editor-tooling audience, and it names only one of three techniques (misses legend interning + int-packing). Kept as the in-prose word for the layout, not the method. |
| **tokens** (`captureTokens`) | — | Mirrors `semanticTokens` for instant familiarity, but **misleads**: this is not a token stream — it keeps grouping (`matchId`) and metadata, exactly the flat-array shape Option B rejects. A wrong mental model baked into the name. |
| **dense** | the *use case* | Reads as "more data", the opposite of the intent (smaller wire). Rejected. |
| **compact** | the *goal* | Clean antonym of verbose, immediately obvious. Close runner-up. |
| **packed** ✓ | the *goal* + a decode step | Chosen. Standard serialization vocabulary, signals "must be unpacked" (it is not self-describing), and pairs as verbose's antonym. |

Rejected `columnar`/`tokens`/`dense`; chose `packed` over the near-equivalent `compact` because it additionally warns the consumer that the payload is not self-describing and needs an unpack step.

## Consequences

### Positive

* Dense kinds shrink dramatically: dropping node identity (~60 B/capture) + flat `range` (~48 B → 16-ish) + name/language legend (~15 B → 1 int) + no per-capture JSON keys takes ~165 B/capture toward tens of bytes — the ~600 KB `full` falls by roughly an order of magnitude, before considering that node minting CPU and tracker pollution vanish too.
* The verbose triple is untouched — no migration, no union, no back-compat tax on the readable surface.
* Grouping, `#set!` metadata, `injection` layering, and skipped-pattern diagnostics are all preserved; the machine format is lossless relative to what the consumer needs.
* Node identity remains addable with zero wire break when a consumer needs it.

### Negative

* A second method family to document and test (mitigated: it is a serializer + lineage slot over shared compute).
* Packed payloads are not human-legible; debugging the machine method means unpacking columns (which is why the verbose triple stays).
* Two lineages per `(uri, kind, mode)` if a client drives both methods — acceptable and independent, like the existing host/injection split.

### Neutral

* Legend is per-result and stable across a lineage; a config reload invalidates the lineage (→ new `full` with a possibly new legend), so the client's re-sync rule stays "on `null`, call `full`".
* The `nodes` lineage axis exists from day one but is always `false` until node columns ship — no behavior today, clean slot separation tomorrow.
* Delta remains match-granular and positional; the packed `data` block is the only new serialization surface, and it references the shared legend.
