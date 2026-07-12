# Node Reference Protocol

**Related Decisions**:
- [lazy-node-identity-tracking](lazy-node-identity-tracking.md) — Underlying identity tracking algorithm
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md) — Virtual document model for injection regions

## Context and Problem Statement

Kakehashi already parses every open document with tree-sitter and maintains stable node identities across edits via lazy-node-identity-tracking. The syntax tree carries information that editor users and plugin authors routinely want to act on — node boundaries, types, parent-child relations, injection structure — but **none of it is currently reachable from the client side**.

The goal of this protocol is to **open kakehashi as a platform for syntax-aware extensions**: any LSP client (editor command, plugin, REPL, scripting layer) should be able to ask kakehashi about nodes and build features on top, **without bundling tree-sitter on the client side** and without re-implementing the parser, queries, or injection logic that kakehashi already maintains.

Concrete extension scenarios this protocol enables:

1. **Structural selection / motions** — "select the enclosing function", "expand to parent block", "shrink to first child"
2. **AST-aware refactoring** — operate on a specific node identified once, even as the surrounding text changes
3. **Long-lived references** — hold a handle to a node across `didChange` events (e.g., for bookmarks, breakpoints, AI agent reasoning)
4. **Injection-aware tooling** — explicitly target a host-language node vs. an injected-language node at the same position

These reduce to a small set of primitives: **identify a node at a position**, **navigate to its parent or children** (with a named-only variant), and **read its current text**. We expose them as custom methods under the `kakehashi/` namespace so that clients can compose richer features on top.

## Decision Drivers

* **Minimal API surface**: Each method returns exactly one kind of information, composing well
* **Edit-resilient**: A held ID must continue to resolve after `didChange`, as long as the underlying node survives invalidation rules (lazy-node-identity-tracking)
* **LSP-spec aligned**: Custom methods, but parameter shapes follow LSP conventions (`TextDocumentIdentifier`, `Position`, `Range`)
* **Predictable null semantics**: A single null response means "not currently valid", whether due to invalidation, prior unknown ID, or out-of-range position
* **Injection-aware**: Multi-layer language nesting (e.g., Markdown → Python → regex) must be addressable explicitly

## Decision Outcome

**Chosen approach**: Introduce five custom LSP methods (`kakehashi/node`, `kakehashi/node/parent`, `kakehashi/node/children`, `kakehashi/node/namedChildren`, `kakehashi/node/text`) returning a minimal `NodeInfo` type and propagating `null` for any unresolvable reference. Injection layer selection is controlled by an explicit `injection` parameter on the entry-point method. Named-vs-anonymous selection follows tree-sitter's own API surface: a `namedOnly` parameter on the entry point (mirroring `named_descendant_for_byte_range`) and a dedicated `namedChildren` navigation method (mirroring `named_children()`).

### Method Catalog

| Method | Input | Output | Purpose |
|---|---|---|---|
| `kakehashi/node` | `{ textDocument, position, injection?, namedOnly? }` | `NodeInfo \| null` | Entry point: position → node identity |
| `kakehashi/node/parent` | `{ textDocument, id }` | `NodeInfo \| null` | Walk one step toward the root |
| `kakehashi/node/children` | `{ textDocument, id }` | `NodeInfo[] \| null` | List immediate children (named + anonymous) |
| `kakehashi/node/namedChildren` | `{ textDocument, id }` | `NodeInfo[] \| null` | List immediate **named** children only |
| `kakehashi/node/text` | `{ textDocument, id }` | `{ text: string } \| null` | Resolve current text content |

All methods carry a `TextDocumentIdentifier` even when an `id` (ULID) is provided. While ULIDs are globally unique by construction, the `textDocument` field keeps the protocol aligned with LSP conventions, allows the server to route directly to the correct per-URI tracker, and lets the server reject mismatched (`uri`, `id`) pairs as `null` instead of silently querying another document.

Beyond these core methods, a family of **node accessor methods** mirrors tree-sitter's [`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html) API one-for-one (`kind`, `childCount`, `child`, `nextSibling`, `childByFieldName`, …) so clients can introspect and walk the tree without bundling tree-sitter — see [Node Accessor Methods](#node-accessor-methods).

### `NodeInfo` Type

```typescript
type NodeInfo = {
  id: string;    // ULID issued per lazy-node-identity-tracking
  kind: string;  // tree-sitter node kind (e.g., "fenced_code_block")
};
```

The field is named `kind`, matching the Rust binding (`node.kind()`) that the server is built on. Keeping the wire name aligned with the implementation removes the type/kind translation at the serialization boundary and keeps the protocol vocabulary consistent with the rest of Kakehashi, which already speaks of a node's `(start, end, kind)` triple throughout the tracker and injection layers.

Positional information (`range`) is **intentionally omitted from `NodeInfo`** for two reasons:

1. **Orthogonality**: positions are served by the dedicated `range` / `startPosition` / `endPosition` accessors (see [Node Accessor Methods](#node-accessor-methods)), mirroring the `text` endpoint shape, rather than riding inline on every `NodeInfo`
2. **Cheap navigation**: `parent`/`children` traversals do not need to resolve positions, keeping per-call cost low

The cost is N+1 round trips for clients that need ranges of every child, accepted as an explicit trade-off. A bulk endpoint (e.g., `childrenWithRange`) may be added later if profiling shows it is needed.

Namedness (`is_named()`) is likewise **not** a field on `NodeInfo`. Named-vs-anonymous is expressed through *selection* — the `namedOnly` parameter and the `namedChildren` method below — rather than reported per node, because the round-trip-sensitive use cases all want to *select* named nodes, not to *introspect* one already in hand. A `named: boolean` field remains available as a future addition: unlike `range` it is intrinsic, immutable, and one byte, so it would belong inline alongside `kind` rather than behind its own endpoint. It is deferred until a concrete consumer needs to label nodes it already holds (e.g. a nearest-named-ancestor `parent` walk) — see Alternatives.

### Entry-Point Method: `kakehashi/node`

```jsonc
// Request
{
  "textDocument": { "uri": "file:///foo.md" },
  "position":     { "line": 3, "character": 5 },
  "injection":    true,    // optional, default: false
  "namedOnly":    false    // optional, default: false
}

// Response: NodeInfo | null
{ "id": "01HX...", "kind": "fenced_code_block" }
```

**Resolution rule**: Returns the **smallest (deepest) node** containing `position` at the selected injection layer. With `namedOnly` absent or `false`, this is the smallest named *or anonymous* node (`descendant_for_byte_range`); with `namedOnly: true`, it is the smallest *named* node (`named_descendant_for_byte_range`) — see The `namedOnly` Parameter below. Returns `null` when the position is outside the document or the requested injection layer does not exist at that position.

### The `injection` Parameter

```typescript
type InjectionSelector = boolean | number;
```

Semantics, given an injection stack `[host, layer₁, layer₂, ..., deepest]` at the cursor position:

| Value | Resolved layer | When stack is shallower |
|---|---|---|
| `false` (default) or `0` | host (layer 0) | (always succeeds — host always exists) |
| `true` | deepest layer (saturating) | (always succeeds — saturates to deepest) |
| Positive `n` (`n ≥ 1`) | exactly layer `n` (`stack[n]`) | `null` |
| Negative `n` (`n ≤ -1`) | `n`-th layer from deepest (`stack[stack.len + n]`) | `null` |

**`true` as saturation shorthand**: All integer indices resolve through a single formula — `stack[n]` for positive `n`, `stack[stack.len + n]` for negative `n` — returning `null` when the index is out of bounds. `true` is a convenience that explicitly saturates to the deepest layer, expressing intent ("go as deep as you can") without requiring the client to know the stack depth. The value `-1` happens to behave like `true` here because every in-bounds cursor has an injection stack containing at least the host (`stack.len ≥ 1`), so `stack[stack.len - 1]` is always valid — but conceptually they take different routes to the same result.

**Example** (Markdown containing a Python code block containing a regex literal):

| Cursor location | `injection` | Returns |
|---|---|---|
| inside Markdown prose | `false` / `0` | Markdown leaf node |
| inside Markdown prose | `1` | `null` (no injection here) |
| inside Python regex literal | `false` / `0` | Markdown `code_fence_content` |
| inside Python regex literal | `1` | Python node |
| inside Python regex literal | `2` / `true` / `-1` | regex node |
| inside Python regex literal | `3` | `null` (stack only 3 layers: 0/1/2) |
| inside Python regex literal | `-2` | Python node |

### The `namedOnly` Parameter

```typescript
type NamedOnly = boolean; // default: false
```

Controls whether the entry point resolves through tree-sitter's anonymous-inclusive or named-only descendant lookup, **at the injection layer already selected by `injection`** (the two parameters compose):

| Value | Resolved node | tree-sitter primitive |
|---|---|---|
| `false` (default) | smallest named **or anonymous** node containing the byte | `descendant_for_byte_range(b, b)` |
| `true` | smallest **named** node containing the byte | `named_descendant_for_byte_range(b, b)` |

**Why a parameter here (but a method for children)**: the entry point already carries the `injection` policy, and named-vs-anonymous is a second, *composing* axis (`named_descendant` within injection layer `n`). Splitting it into a separate method would duplicate the protocol's most complex handler once per `injection × namedOnly` combination. Navigation methods have no such second axis, so they follow tree-sitter's own surface instead — a dedicated `namedChildren` method rather than a flag (see Navigation Methods).

**Equivalence to a parent-walk**: `namedOnly: true` returns exactly the node a client would reach by resolving the anonymous-inclusive node and walking `parent` until the first named ancestor. The nodes containing a given byte form a single ancestor chain (tree-sitter siblings are non-overlapping), so "smallest named node containing `b`" *is* the first named node on the way up. The parameter collapses that N-round-trip walk — which would also mint a ULID for every anonymous node passed through, against lazy-node-identity-tracking's bounded-memory goal — into one native lookup that mints exactly one ID.

**Primary use case**: parity with editor defaults such as Neovim's `vim.treesitter.get_node{ include_anonymous = false }`, which resolves the smallest *named* node. Such clients pass `namedOnly: true`. The protocol default stays `false` so the entry point exposes the full tree (consistent with `children` returning anonymous nodes too) and lets clients narrow explicitly, rather than baking one editor's default into the protocol.

### Boundary Semantics

#### Position Encoding

The boundary rules below are stated in terms of UTF-8 byte offsets (`b`, `[s, e)`, `L`), matching the representation used by tree-sitter internally. The API surface, however, accepts LSP `Position` values whose `character` field unit depends on the negotiated `positionEncoding`.

**Kakehashi uses UTF-16 exclusively**: when a client advertises `general.positionEncodings`, the server selects and announces `positionEncoding: "utf-16"`; when the capability is omitted, both sides use the LSP default of UTF-16. The server converts each incoming `Position` to a UTF-8 byte offset (via `PositionMapper`) before applying any boundary rule. This conversion is transparent to clients, but two consequences matter:

- For documents containing only ASCII, UTF-16 code units, UTF-8 bytes, and characters coincide — most boundary discussions remain intuitive.
- For documents with non-ASCII characters (multi-byte in UTF-8, multi-code-unit in UTF-16 for surrogate pairs), clients must compute `Position.character` in UTF-16 code units. Sending a byte-based character count will misalign with the byte ranges resolved server-side, especially around emoji or CJK glyphs at injection or node boundaries.

Clients must support UTF-16 as required by LSP even when they also advertise UTF-8 or UTF-32. Kakehashi may support selecting those optional encodings in the future, but currently always selects UTF-16.

#### Half-Open Intervals

All position-to-node resolution uses **half-open intervals** `[start, end)`:

- A cursor at byte `b` lies inside a node `[s, e)` iff `s ≤ b < e`
- A cursor exactly at the end byte of a node is **not** inside that node
- Injection layer membership uses the same rule against the injection's included ranges

This makes "cursor at the closing fence of a code block" unambiguously **outside** the injection.

#### End-of-Document Exception

LSP positions allow the cursor to sit **after the last byte** of the document (`b == L`, where `L` is the document length). This is the position users reach via End-of-file motions and is naturally produced when appending text. Under the strict half-open rule, no node — not even the root — would contain `b == L`, so every node query at end-of-document would return `null`. This breaks AST-walking commands at end-of-file.

The boundary rule is therefore relaxed by exactly one case, **gated on `L > 0`**:

> A cursor at byte `b` is contained by a node `[s, e)` iff `s ≤ b < e`, **OR** (`L > 0` and `b == L` and `e == L`).

Properties of this rule:

- **Position is not modified**: unlike clamping `b` to `L - 1`, the cursor stays at `L`. This avoids surprising behavior when the last byte is a newline (clamping would place the cursor "inside the trailing newline").
- **Only fires at document end of a non-empty document**: the exception requires `e == L` exactly and `L > 0`. Interior boundaries (cursor at the end of an injection but before the document end) keep the unmodified half-open behavior, so "cursor at closing fence" still falls out of the injection into the host.
- **Smallest-wins still applies**: multiple nested nodes typically end at `L` (root → top-level block → last statement → ...). The standard "smallest containing node" rule selects the deepest among them.
- **Empty document is excluded by construction**: if `L == 0`, the exception clause never fires regardless of any node's span, so empty documents always return `null` (see edge cases below). This avoids returning a degenerate root node spanning `[0, 0)`.

Edge cases:

| Cursor position | Behavior |
|---|---|
| `b == L`, document non-empty (`L > 0`) | Returns the smallest node with `e == L` (typically the rightmost branch down from root) |
| Empty document (`L == 0`) | Exception is gated off by `L > 0`; returns `null` |
| `b > L` (out of bounds) | Returns `null` |

### Navigation Methods

```jsonc
// kakehashi/node/parent
{ "textDocument": { "uri": "..." }, "id": "01HX..." }    →    NodeInfo | null

// kakehashi/node/children        — named + anonymous
{ "textDocument": { "uri": "..." }, "id": "01HX..." }    →    NodeInfo[] | null

// kakehashi/node/namedChildren   — named only
{ "textDocument": { "uri": "..." }, "id": "01HX..." }    →    NodeInfo[] | null
```

**Scope rule**: Navigation stays within a single language tree. Calling `parent` on the root of an injected tree returns `null`, **not** the host node that contains the injection. Crossing injection boundaries requires a fresh `kakehashi/node` call.

This holds even when a host node and an injected node share an identical span **and** kind (e.g. recursive same-language injection such as markdown-in-markdown). The identity key carries an injection-`layer` discriminator (lazy-node-identity-tracking § Node Uniqueness Key), so the two are distinct ULIDs and `parent`/`children` resolve each in the tree that minted it.

The layer discriminator identifies a **depth**, not a same-depth region. When
multiple overlapping injection regions at the same depth contain the requested
position, `kakehashi/node` returns `null` rather than minting an identity from an
arbitrary sibling. Accessors likewise return `null` for an already-held ID when
rebuilding its path becomes ambiguous. Clients must re-acquire after edits, but
re-acquisition can remain unavailable while the overlap itself persists.

**`null` cases**:
- `parent`: id not in tracker, id refers to a root node, **or** its injection layer is ambiguous
- `children` / `namedChildren`: id not in tracker, **or** its injection layer is ambiguous

**Empty children**: A node that exists but has no children (or no *named* children, for `namedChildren`) returns `[]` (not `null`).

**Ordering**: Children are returned in **document order** — equivalent to ascending `start_byte` because direct siblings in a tree-sitter tree are non-overlapping by construction. This matches tree-sitter's native child iteration and gives clients a deterministic walk order for structural navigation, AST walks, fold computation, and "go to next/previous sibling" gestures. The ordering invariant is preserved across the named-only variant (`namedChildren`) and any future filtering: narrowing the sequence never reorders it.

**Named vs anonymous**: `children` returns both named and anonymous children; `namedChildren` returns only the named ones, mirroring tree-sitter's `children()` vs `named_children()`. It is a **separate method, not a `namedOnly` flag on `children`**, for two reasons: (1) it matches tree-sitter's own API surface, which blesses exactly the named distinction (there is no `extra_children()` etc.), so there is no flag combinatorics to absorb; (2) filtering server-side returns only the named nodes, so the tracker mints ULIDs **only** for children the client keeps — filtering a full `children` result client-side would instead mint and retain IDs for anonymous children the client immediately discards, against lazy-node-identity-tracking's bounded-memory goal.

**No `namedParent`**: `parent` has no named-only variant, because tree-sitter's `Node` has no `named_parent` — `parent()` is singular. A client wanting the nearest *named* ancestor walks `parent` and stops at the first named node. Knowing which nodes are named for that walk is the one case that would motivate a `named` field on `NodeInfo` (mirroring `is_named()`); it is deferred until such a consumer exists (see Alternatives).

### Node Accessor Methods

To let clients introspect and walk the tree without re-implementing tree-sitter, the protocol exposes an accessor family mirroring [`tree_sitter::Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html) method-for-method. Every accessor takes `{ textDocument, id }` (plus, where the tree-sitter method does, an `index` / `name` / `byte` / `startByte` + `endByte`) and resolves the held ULID **in the layer that minted it**, so the per-layer Scope rule holds uniformly: results of a navigation accessor are re-minted in the same layer as the input node.

**Scalar accessors** return a single-field object (matching `text`'s `{ "text": ... }` shape — self-describing and additively extensible), or top-level `null` for an unresolvable id:

| Method | Response | tree-sitter |
|---|---|---|
| `kind` | `{ "kind": string }` | `kind()` |
| `grammarName` | `{ "grammarName": string }` | `grammar_name()` |
| `isNamed` / `isExtra` | `{ "isNamed": bool }` / `{ "isExtra": bool }` | `is_named()` / `is_extra()` |
| `hasError` / `isError` / `isMissing` | `{ "hasError": bool }` … | `has_error()` / `is_error()` / `is_missing()` |
| `startByte` / `endByte` | `{ "startByte": int }` / `{ "endByte": int }` | `start_byte()` / `end_byte()` |
| `byteRange` | `{ "startByte": int, "endByte": int }` | `byte_range()` |
| `childCount` / `namedChildCount` | `{ "childCount": int }` … | `child_count()` / `named_child_count()` |
| `descendantCount` | `{ "descendantCount": int }` | `descendant_count()` |
| `toSexp` | `{ "sexp": string }` | `to_sexp()` |

**Byte offsets are UTF-8** in host-document coordinates — tree-sitter's native space, the same one `text` slices and the byte-input accessors consume. This deliberately differs from `Position.character` (UTF-16); line/column reporting lives in the position/range accessors below, which speak LSP `Position`.

**Navigation accessors** return `NodeInfo | null` (single) or `NodeInfo[] | null` (list — `null` only for an unresolvable id; an empty relation yields `[]`):

| Method | Response | tree-sitter |
|---|---|---|
| `child` / `namedChild` (`index`) | `NodeInfo \| null` | `child(i)` / `named_child(i)` |
| `namedChildren` | `NodeInfo[] \| null` | `named_children()` |
| `nextSibling` / `prevSibling` | `NodeInfo \| null` | `next_sibling()` / `prev_sibling()` |
| `nextNamedSibling` / `prevNamedSibling` | `NodeInfo \| null` | `next_named_sibling()` / `prev_named_sibling()` |
| `firstChildForByte` (`byte`) | `NodeInfo \| null` | `first_child_for_byte(b)` |
| `descendantForByteRange` (`startByte`, `endByte`) | `NodeInfo \| null` | `descendant_for_byte_range(s, e)` |
| `namedDescendantForByteRange` (`startByte`, `endByte`) | `NodeInfo \| null` | `named_descendant_for_byte_range(s, e)` |
| `childWithDescendant` (`descendantId`) | `NodeInfo \| null` | `child_with_descendant(d)` |

Out-of-range / negative `index` or `byte` values collapse to `null` rather than erroring, consistent with the universal null semantics.

`childWithDescendant` is the only **two-id** accessor: `{ textDocument, id, descendantId }` resolves both ids and returns the immediate child of `id` that contains `descendantId`. Both ids must have been minted in the **same** injection layer — both nodes must live in one tree for the ancestor/descendant relation to be meaningful, so a cross-layer pair collapses to `null` rather than resolving either id against the other's layer. tree-sitter leaves `child_with_descendant` undefined when the argument is not actually a descendant (byte-containment ties on equal-range unary chains); the server verifies the relation during the descent and normalizes every unrelated pair — including `descendantId == id` — to the universal `null`.

**Position / range accessors** report and accept line/column as LSP `Position` (`{ line, character }`), **not** tree-sitter's native `Point`:

| Method | Input | Response | tree-sitter |
|---|---|---|---|
| `startPosition` | `{ id }` | `{ "startPosition": Position }` | `start_position()` |
| `endPosition` | `{ id }` | `{ "endPosition": Position }` | `end_position()` |
| `range` | `{ id }` | `{ "start": Position, "end": Position }` | `range()` |
| `descendantForPointRange` | `{ id, start, end }` | `NodeInfo \| null` | `descendant_for_point_range(s, e)` |
| `namedDescendantForPointRange` | `{ id, start, end }` | `NodeInfo \| null` | `named_descendant_for_point_range(s, e)` |

**Why LSP `Position`, not tree-sitter `Point`**: tree-sitter's `Point.column` is a **UTF-8 byte** offset within the line, whereas the protocol speaks LSP `Position` (UTF-16 code units) everywhere else (§"Position Encoding"). Returning native points would diverge from every other LSP coordinate around non-ASCII (emoji, CJK) and force clients to special-case these accessors. Instead the server converts each end via `PositionMapper`: byte → `Position` on output, `Position` → byte on input (reusing the byte-range search and its inverted / out-of-bounds guards). Clients that genuinely want byte-native spans use `startByte` / `endByte` / `byteRange` and `descendant*ForByteRange`, which are unambiguous. A `Position` that cannot be mapped into the current document collapses to `null`.

This supersedes the earlier deferral of range reporting (Alternative C): rather than a single bulk `kakehashi/node/range` endpoint, each tree-sitter method is exposed individually for 1:1 parity with the `Node` API; bulk range retrieval (e.g. `childrenWithRange`) remains a possible future addition if profiling shows the N+1 cost dominates.

**Field accessors** expose the name-keyed half of tree-sitter's field API:

| Method | Response | tree-sitter |
|---|---|---|
| `childByFieldName` (`name`) | `NodeInfo \| null` | `child_by_field_name(name)` |
| `childrenByFieldName` (`name`) | `NodeInfo[] \| null` | `children_by_field_name(name)` |
| `fieldNameForChild` (`index`) | `{ "fieldName": string \| null } \| null` | `field_name_for_child(i)` |
| `fieldNameForNamedChild` (`index`) | `{ "fieldName": string \| null } \| null` | `field_name_for_named_child(i)` |

For `fieldNameFor*`, top-level `null` means the *id* is unresolvable, while `{ "fieldName": null }` means the node resolved but that child carries no field — the two are deliberately distinguished.

### Text Resolution: `kakehashi/node/text`

```jsonc
// Request
{ "textDocument": { "uri": "..." }, "id": "01HX..." }

// Response
{ "text": "print(\"hello world\")" }   // node is live; text reflects post-edit content
null                                    // id is not currently valid for this document
```

The returned text is sliced from the **current** document content using the node's adjusted byte range. Because lazy-node-identity-tracking's position adjustment runs synchronously inside `didChange`, the slice is always consistent with the document the client has observed.

### Invalidate vs Not-Found

Clients **cannot distinguish** between:

- An ID that was invalidated by an edit (START fell inside the edit range)
- An ID that was never issued by this server for the supplied `textDocument`
- A `textDocument` that has no tracker entries (e.g., never opened, already closed)

All three collapse to `null`. This is a deliberate consequence of the no-tombstone, no-LRU design (lazy-node-identity-tracking). Clients that need to refresh a node should treat `null` as "re-acquire via `kakehashi/node`".

### Lifecycle

- `didOpen`: no eager work — IDs are issued lazily on first `kakehashi/node*` request
- `didChange`: existing IDs are repositioned or invalidated per lazy-node-identity-tracking
- `didClose`: all IDs for the URI are dropped

## Example Flow

```
1. Client: kakehashi/node { textDocument, position, injection: true }
2. Server: { id: "01HX-A", type: "call" }

3. ... user edits, didChange fires ...

4. Client: kakehashi/node/text { textDocument, id: "01HX-A" }
5. Server: { text: "foo(updated_arg)" }     // ID survived, text reflects edit

6. Client: kakehashi/node/parent { textDocument, id: "01HX-A" }
7. Server: { id: "01HX-B", type: "expression_statement" }

8. Client: kakehashi/node/children { textDocument, id: "01HX-B" }
9. Server: [ { id: "01HX-A", type: "call" } ]
```

## Consequences

### Positive

- **Stable references across edits**: Clients can hold a node ID through editing sessions
- **Composable API**: Five orthogonal methods cover position lookup, navigation (named + anonymous, or named-only), and content retrieval
- **No tree-sitter on client**: Editors can implement syntax-aware features without bundling tree-sitter
- **Lazy memory growth**: Only nodes touched via the protocol consume tracker memory; `namedChildren` further avoids minting IDs for anonymous children a client would discard
- **Injection-explicit**: Host-vs-injected and unambiguous nested layers are addressable by depth
- **tree-sitter-faithful named selection**: `namedOnly` and `namedChildren` map 1:1 to `named_descendant_for_byte_range` / `named_children()`, giving editor-default parity (e.g. Neovim `get_node`) in one round trip with no extra ID churn
- **Predictable error model**: `null` is the universal "not currently resolvable" signal

### Negative

- **N+1 for positions**: Clients building outline views must call `range` once per node (a bulk `childrenWithRange` remains a possible future addition), or accept paying for it only when needed
- **No invalidate diagnostics**: Clients cannot tell why an ID failed; they must re-acquire blindly
- **Cross-injection navigation is two-step**: `parent` does not transparently cross into a host tree
- **Overlapping same-depth injections are not addressable**: A depth-only identity cannot distinguish sibling regions, so lookup and accessors fail closed with `null`
- **`injection` mixed mode**: `true` saturates while integer indices are strict (resolve via a single formula, return `null` when out of bounds) — clients must remember which mode they want
- **Unbounded tracker growth per URI**: Long editing sessions on large files accumulate IDs until `didClose` (no LRU per lazy-node-identity-tracking)

### Neutral

- **Custom methods under `kakehashi/`**: Consistent with existing extensions (`kakehashi/internal/effectiveConfiguration`)
- **Named-vs-anonymous surface mirrors tree-sitter**: parameter on the entry point (`named_descendant_for_byte_range`), dedicated method for children (`named_children()`), no variant for `parent` (the `is_named()` field is deferred)
- **Half-open intervals**: Standard convention for ranges in most text APIs

## Alternatives Considered

### Alternative A: Single Monolithic `nodeInfo` Method

Return `{ id, type, range, text, parent, children }` in one response.

* Bad, because **violates orthogonality** — clients pay for data they don't need
* Bad, because **`children` payload explodes** for large containers (a `module` node with hundreds of statements)

### Alternative B: Position-Based References (No IDs)

Identify nodes by `(uri, start_byte, end_byte, type)` directly.

* Bad, because **does not survive edits** — byte positions shift after `didChange`
* Bad, because **clients must track edits** themselves to update references
* Equivalent to lazy-node-identity-tracking's Option 2, rejected for the same reasons

### Alternative C: Embed Range in `NodeInfo`

Include `range` in every `NodeInfo` returned.

* Acceptable trade-off, but rejected in favor of orthogonal `range` / `startPosition` / `endPosition` accessors (now implemented — see [Node Accessor Methods](#node-accessor-methods))
* Reconsider a bulk variant (`childrenWithRange`) if profiling shows the N+1 round-trip cost dominates

### Alternative D: Auto-Cross Injection in `parent`

`parent` of an injected root returns the host's injection container node.

* Bad, because **opaque layer crossing surprises clients** — they cannot tell whether they're still in the same language tree
* Bad, because **breaks tree-sitter's logical separation** between host and injected trees

### Alternative E: Uniform `namedOnly` Parameter on Every Navigation Method

Add `namedOnly` to `parent` and `children` (and entry) instead of a dedicated `namedChildren` method.

* Bad, because **does not match tree-sitter's surface** — tree-sitter exposes `named_children()` as a method and has **no** `named_parent`, so a uniform flag invents a `parent` variant with no tree-sitter counterpart
* Bad, because **invites flag combinatorics** on navigation methods (a later `extra` / `error` / `missing` filter would each add another boolean), whereas tree-sitter blesses only the named distinction
* The entry point is the deliberate exception: it already carries `injection`, and named-vs-anonymous *composes* with it, so a parameter there avoids a method-per-combination explosion

### Alternative F: `children` + `named` Field + Client-Side Filter

Return both children always, add `named: boolean` to `NodeInfo`, and let clients filter for named.

* Bad, because **mints ULIDs for discarded nodes** — resolving every child to filter client-side tracks anonymous children the client throws away, fighting lazy-node-identity-tracking's bounded-memory design
* Bad, because **larger payloads** and per-client filtering logic for a distinction the server can make natively via `named_children()`
* The `named` field itself is not rejected outright — it is deferred until a *holding-a-node* consumer (e.g. a nearest-named-ancestor `parent` walk) needs it, at which point it would live inline on `NodeInfo`

### Alternative G: Separate `kakehashi/node/named` Entry Method

Split the entry point into anonymous-inclusive and named-only methods instead of a `namedOnly` parameter.

* Bad, because **duplicates the most complex handler** — both would carry the full `injection` resolution, parser auto-install, and re-snapshot machinery
* Bad, because **injection × named is a 2-D space** better expressed as two composing parameters than four method names

## Implementation Notes

- Methods are registered via `LspService::build().custom_method(...)` in `src/bin/main.rs`, following the existing `kakehashi/internal/effectiveConfiguration` pattern
- Handlers live under `src/lsp/lsp_impl/kakehashi/node/`. The core methods keep one file each (`entry`, `text`, `parent`, `children`); the accessor family is grouped by category (`metadata`, `navigation`, `field`) since each method shrinks to a few lines over the shared `common` prelude (`with_node_by_id` / `navigate_to_node` / `navigate_to_nodes`), which centralises URI/ULID resolution, parse-readiness, snapshotting, and per-layer node resolution
- The entry point resolves `namedOnly` by switching `descendant_for_byte_range` → `named_descendant_for_byte_range` at the selected layer's tree; the end-of-document exception walks the right spine to the deepest node ending at `L`, restricted to named nodes when `namedOnly` is set
- `namedChildren` reuses the `children` handler's tracker-minting path over `Node::named_children` instead of `Node::children`
- `NodeTracker` (`src/language/node_tracker.rs`) backs all four id-based methods via a per-URI bidirectional index — forward (`PositionKey → Ulid`) for minting/dedup, reverse (`Ulid → PositionKey`) for resolving a held ULID back to a node range; see lazy-node-identity-tracking
- Injection layer enumeration reuses the existing injection processing in `src/lsp/lsp_impl/coordinator/injection.rs`

## Summary

| Aspect | Decision |
|--------|----------|
| **Methods** | `kakehashi/node`, `/parent`, `/children`, `/namedChildren`, `/text` |
| **Common params** | All methods carry `TextDocumentIdentifier` |
| **`NodeInfo` shape** | `{ id, kind }` (range via separate `range`/`startPosition`/`endPosition` accessors, not inline; `named` deferred) |
| **Entry resolution** | Smallest containing node at selected injection layer |
| **Injection selector** | `boolean \| number`, default `false` |
| **Named selection** | `namedOnly` param on entry (`named_descendant_for_byte_range`); `namedChildren` method (`named_children()`); no `parent` variant (tree-sitter has no `named_parent`) |
| **Boundary** | Half-open `[start, end)`; end-of-document (`b == L`) is contained by nodes with `e == L` |
| **Navigation scope** | Single language tree per call |
| **Null semantics** | Universal "not currently resolvable" |
| **Invalidate diagnostics** | Not provided (collapsed into `null`) |
