# ADR-0025: Node Reference Protocol

| | |
|---|---|
| **Date** | 2026-05-27 |
| **Status** | Draft |
| **Type** | Protocol Extension |

**Related ADRs**:
- [ADR-0019](0019-lazy-node-identity-tracking.md) — Underlying identity tracking algorithm
- [ADR-0007](0007-language-server-bridge-virtual-document-model.md) — Virtual document model for injection regions

## Context and Problem Statement

Kakehashi already parses every open document with tree-sitter and maintains stable node identities across edits via [ADR-0019](0019-lazy-node-identity-tracking.md). The syntax tree carries information that editor users and plugin authors routinely want to act on — node boundaries, types, parent-child relations, injection structure — but **none of it is currently reachable from the client side**.

The goal of this protocol is to **open kakehashi as a platform for syntax-aware extensions**: any LSP client (editor command, plugin, REPL, scripting layer) should be able to ask kakehashi about nodes and build features on top, **without bundling tree-sitter on the client side** and without re-implementing the parser, queries, or injection logic that kakehashi already maintains.

Concrete extension scenarios this protocol enables:

1. **Structural selection / motions** — "select the enclosing function", "expand to parent block", "shrink to first child"
2. **AST-aware refactoring** — operate on a specific node identified once, even as the surrounding text changes
3. **Long-lived references** — hold a handle to a node across `didChange` events (e.g., for bookmarks, breakpoints, AI agent reasoning)
4. **Injection-aware tooling** — explicitly target a host-language node vs. an injected-language node at the same position

These all reduce to four primitives: **identify a node at a position**, **navigate to its parent or children**, and **read its current text**. We expose them as custom methods under the `kakehashi/` namespace so that clients can compose richer features on top.

## Decision Drivers

* **Minimal API surface**: Each method returns exactly one kind of information, composing well
* **Edit-resilient**: A held ID must continue to resolve after `didChange`, as long as the underlying node survives invalidation rules ([ADR-0019](0019-lazy-node-identity-tracking.md))
* **LSP-spec aligned**: Custom methods, but parameter shapes follow LSP conventions (`TextDocumentIdentifier`, `Position`, `Range`)
* **Predictable null semantics**: A single null response means "not currently valid", whether due to invalidation, prior unknown ID, or out-of-range position
* **Injection-aware**: Multi-layer language nesting (e.g., Markdown → Python → regex) must be addressable explicitly

## Decision Outcome

**Chosen approach**: Introduce four custom LSP methods (`kakehashi/node`, `kakehashi/node/parent`, `kakehashi/node/children`, `kakehashi/node/text`) returning a minimal `NodeInfo` type and propagating `null` for any unresolvable reference. Injection layer selection is controlled by an explicit `injection` parameter on the entry-point method.

### Method Catalog

| Method | Input | Output | Purpose |
|---|---|---|---|
| `kakehashi/node` | `{ textDocument, position, injection? }` | `NodeInfo \| null` | Entry point: position → node identity |
| `kakehashi/node/parent` | `{ textDocument, id }` | `NodeInfo \| null` | Walk one step toward the root |
| `kakehashi/node/children` | `{ textDocument, id }` | `NodeInfo[] \| null` | List immediate children |
| `kakehashi/node/text` | `{ textDocument, id }` | `{ text: string } \| null` | Resolve current text content |

All methods carry a `TextDocumentIdentifier` even when an `id` (ULID) is provided. While ULIDs are globally unique by construction, the `textDocument` field keeps the protocol aligned with LSP conventions, allows the server to route directly to the correct per-URI tracker, and lets the server reject mismatched (`uri`, `id`) pairs as `null` instead of silently querying another document.

### `NodeInfo` Type

```typescript
type NodeInfo = {
  id: string;    // ULID issued per ADR-0019
  type: string;  // tree-sitter node type (e.g., "fenced_code_block")
};
```

The field is named `type` (matching the tree-sitter web binding) rather than `kind` (the Rust binding). Kakehashi's LSP API surface is consumed predominantly by editor plugins in TypeScript/JavaScript, where `node.type` is the established convention. LSP's own `*Kind` types denote fixed enums (e.g., `SymbolKind.Function = 12`), whereas this field carries an open grammar-derived string — semantically closer to a structural "type" than to an enumerated "kind".

Positional information (`range`) is **intentionally omitted** for two reasons:

1. **Orthogonality**: A future `kakehashi/node/range` endpoint can return positions when needed, mirroring the `text` endpoint shape
2. **Cheap navigation**: `parent`/`children` traversals do not need to resolve positions, keeping per-call cost low

The cost is N+1 round trips for clients that need ranges of every child, accepted as an explicit trade-off. A bulk endpoint (e.g., `childrenWithRange`) may be added later if profiling shows it is needed.

### Entry-Point Method: `kakehashi/node`

```jsonc
// Request
{
  "textDocument": { "uri": "file:///foo.md" },
  "position":     { "line": 3, "character": 5 },
  "injection":    true     // optional, default: false
}

// Response: NodeInfo | null
{ "id": "01HX...", "type": "fenced_code_block" }
```

**Resolution rule**: Returns the **smallest (deepest) named or anonymous node** containing `position` at the selected injection layer. Returns `null` when the position is outside the document or the requested injection layer does not exist at that position.

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

### Boundary Semantics

#### Position Encoding

The boundary rules below are stated in terms of UTF-8 byte offsets (`b`, `[s, e)`, `L`), matching the representation used by tree-sitter internally. The API surface, however, accepts LSP `Position` values whose `character` field unit depends on the negotiated `positionEncoding`.

**Kakehashi follows the LSP default**: the server does not advertise an explicit `positionEncoding` in `ServerCapabilities`, so per LSP 3.18 spec the client must treat `Position.character` as a **UTF-16 code unit** offset. The server converts each incoming `Position` to a UTF-8 byte offset (via `PositionMapper`) before applying any boundary rule. This conversion is transparent to clients, but two consequences matter:

- For documents containing only ASCII, UTF-16 code units, UTF-8 bytes, and characters coincide — most boundary discussions remain intuitive.
- For documents with non-ASCII characters (multi-byte in UTF-8, multi-code-unit in UTF-16 for surrogate pairs), clients must compute `Position.character` in UTF-16 code units. Sending a byte-based character count will misalign with the byte ranges resolved server-side, especially around emoji or CJK glyphs at injection or node boundaries.

If a client requires a different encoding (UTF-8 or UTF-32), it should negotiate via `general.positionEncodings` in the initialize request. Kakehashi may choose to support those in the future, but currently does not.

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

// kakehashi/node/children
{ "textDocument": { "uri": "..." }, "id": "01HX..." }    →    NodeInfo[] | null
```

**Scope rule**: Navigation stays within a single language tree. Calling `parent` on the root of an injected tree returns `null`, **not** the host node that contains the injection. Crossing injection boundaries requires a fresh `kakehashi/node` call.

**`null` cases**:
- `parent`: id not in tracker, **or** id refers to a root node
- `children`: id not in tracker

**Empty children**: A node that exists but has no children returns `[]` (not `null`).

**Named vs anonymous**: `children` returns both named and anonymous children. A future request parameter (e.g., `namedOnly`) may restrict this if needed.

### Text Resolution: `kakehashi/node/text`

```jsonc
// Request
{ "textDocument": { "uri": "..." }, "id": "01HX..." }

// Response
{ "text": "print(\"hello world\")" }   // node is live; text reflects post-edit content
null                                    // id is not currently valid for this document
```

The returned text is sliced from the **current** document content using the node's adjusted byte range. Because [ADR-0019](0019-lazy-node-identity-tracking.md)'s position adjustment runs synchronously inside `didChange`, the slice is always consistent with the document the client has observed.

### Invalidate vs Not-Found

Clients **cannot distinguish** between:

- An ID that was invalidated by an edit (START fell inside the edit range)
- An ID that was never issued by this server for the supplied `textDocument`
- A `textDocument` that has no tracker entries (e.g., never opened, already closed)

All three collapse to `null`. This is a deliberate consequence of the no-tombstone, no-LRU design ([ADR-0019](0019-lazy-node-identity-tracking.md)). Clients that need to refresh a node should treat `null` as "re-acquire via `kakehashi/node`".

### Lifecycle

- `didOpen`: no eager work — IDs are issued lazily on first `kakehashi/node*` request
- `didChange`: existing IDs are repositioned or invalidated per [ADR-0019](0019-lazy-node-identity-tracking.md)
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
- **Composable API**: Four orthogonal methods cover position lookup, navigation, and content retrieval
- **No tree-sitter on client**: Editors can implement syntax-aware features without bundling tree-sitter
- **Lazy memory growth**: Only nodes touched via the protocol consume tracker memory
- **Injection-explicit**: Multi-layer language stacks are addressable without ambiguity
- **Predictable error model**: `null` is the universal "not currently resolvable" signal

### Negative

- **N+1 for positions**: Clients building outline views must call `kakehashi/node/range` (future) once per node, or accept paying for it only when needed
- **No invalidate diagnostics**: Clients cannot tell why an ID failed; they must re-acquire blindly
- **Cross-injection navigation is two-step**: `parent` does not transparently cross into a host tree
- **`injection` mixed mode**: `true` saturates while integer indices are strict (resolve via a single formula, return `null` when out of bounds) — clients must remember which mode they want
- **Unbounded tracker growth per URI**: Long editing sessions on large files accumulate IDs until `didClose` (no LRU per [ADR-0019](0019-lazy-node-identity-tracking.md))

### Neutral

- **Custom methods under `kakehashi/`**: Consistent with existing extensions (`kakehashi/internal/effectiveConfiguration`)
- **Named + anonymous children**: Returning both is a default; can be parameterized later
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
* Equivalent to ADR-0019's Option 2, rejected for the same reasons

### Alternative C: Embed Range in `NodeInfo`

Include `range` in every `NodeInfo` returned.

* Acceptable trade-off, but rejected for now in favor of the orthogonal `kakehashi/node/range` endpoint (future)
* Reconsider if profiling shows the N+1 round-trip cost dominates

### Alternative D: Auto-Cross Injection in `parent`

`parent` of an injected root returns the host's injection container node.

* Bad, because **opaque layer crossing surprises clients** — they cannot tell whether they're still in the same language tree
* Bad, because **breaks tree-sitter's logical separation** between host and injected trees

## Implementation Notes

- Methods are registered via `LspService::build().custom_method(...)` in `src/bin/main.rs`, following the existing `kakehashi/internal/effectiveConfiguration` pattern
- Handlers live under `src/lsp/lsp_impl/kakehashi/node/` (one file per method) for symmetry with `kakehashi/internal/`
- `RegionIdTracker` is renamed to `NodeTracker` and extended with a reverse index (`Ulid → PositionKey`); see [ADR-0019](0019-lazy-node-identity-tracking.md) amendment
- Injection layer enumeration reuses the existing injection processing in `src/lsp/lsp_impl/coordinator/injection.rs`

## Summary

| Aspect | Decision |
|--------|----------|
| **Methods** | `kakehashi/node`, `/parent`, `/children`, `/text` |
| **Common params** | All methods carry `TextDocumentIdentifier` |
| **`NodeInfo` shape** | `{ id, type }` (no range) |
| **Entry resolution** | Smallest containing node at selected injection layer |
| **Injection selector** | `boolean \| number`, default `false` |
| **Boundary** | Half-open `[start, end)`; end-of-document (`b == L`) is contained by nodes with `e == L` |
| **Navigation scope** | Single language tree per call |
| **Null semantics** | Universal "not currently resolvable" |
| **Invalidate diagnostics** | Not provided (collapsed into `null`) |
