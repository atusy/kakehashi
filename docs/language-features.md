# Language Feature Methods (`kakehashi/*`)

Beyond the standard LSP features (semantic tokens, selection range, bridge), kakehashi exposes a set of **custom methods** under the `kakehashi/` namespace. They open the syntax tree kakehashi already maintains to any LSP client — editor commands, plugins, REPLs, scripting layers — so you can build syntax-aware features **without bundling Tree-sitter on the client side**.

These methods are a stable platform surface, not internal plumbing. The design rationale lives in the architecture decision records:

- [Node Reference Protocol](architecture-decisions/node-reference-protocol.md) — the `kakehashi/node*` method family
- [Lazy Node Identity Tracking](architecture-decisions/lazy-node-identity-tracking.md) — how node IDs stay stable across edits

## Method Overview

| Method | Input | Output | Purpose |
|---|---|---|---|
| [`kakehashi/node`](#kakehashinode) | `{ textDocument, position, injection? }` | `NodeInfo \| null` | Resolve a position to a node identity |
| [`kakehashi/node/parent`](#kakehashinodeparent) | `{ textDocument, id }` | `NodeInfo \| null` | Walk one step toward the root |
| [`kakehashi/node/children`](#kakehashinodechildren) | `{ textDocument, id }` | `NodeInfo[] \| null` | List immediate children |
| [`kakehashi/node/text`](#kakehashinodetext) | `{ textDocument, id }` | `{ text } \| null` | Read a node's current text |
| [`kakehashi/internal/effectiveConfiguration`](#kakehashiinternaleffectiveconfiguration) | `{}` | `{ settings }` | Inspect merged configuration |

> **Status note:** `kakehashi/node/namedChildren` and the `namedOnly` parameter on `kakehashi/node` are specified in the Node Reference Protocol ADR but **not yet implemented**. See [Planned Additions](#planned-additions).

## Node Reference Protocol

The `kakehashi/node*` family turns the syntax tree into a queryable, edit-stable graph. The primitives compose into richer features:

- **Structural selection / motions** — "select the enclosing function", "expand to parent block"
- **AST-aware refactoring** — operate on a node identified once, even as surrounding text changes
- **Long-lived references** — hold a handle to a node across `didChange` events (bookmarks, breakpoints, AI-agent reasoning)
- **Injection-aware tooling** — target a host-language node vs. an injected-language node at the same position

### Core Concepts

#### `NodeInfo`

Every node-returning method yields a minimal node descriptor:

```typescript
type NodeInfo = {
  id: string;    // ULID — a stable, opaque handle to this node
  type: string;  // Tree-sitter node type, e.g. "fenced_code_block", "call"
};
```

`type` matches the Tree-sitter web binding's `node.type` (an open, grammar-derived string), not an LSP `*Kind` enum. Positional information (`range`) is intentionally **not** included; navigation stays cheap, and a future `kakehashi/node/range` endpoint can return positions when needed.

#### Stable IDs across edits

The `id` is a ULID that **survives `didChange`** as long as the underlying node survives. A node's identity is anchored to its **start** position: editing a code block's *contents* keeps the block's ID; editing its *opening delimiter* invalidates it. A held ID lets you re-read a node's text or navigate from it after the user has typed, without re-resolving from a position.

#### `null` means "re-acquire"

Every method returns `null` as a single, uniform "not currently resolvable" signal. A client **cannot** distinguish between:

- an ID invalidated by an edit,
- an ID never issued for this document, or
- a document with no tracked nodes (never opened, or already closed).

All three collapse to `null`. The correct client response is always the same: **re-acquire the node via `kakehashi/node`**.

#### Position encoding

Inputs use LSP `Position` values. kakehashi follows the LSP default and does not advertise a `positionEncoding`, so `Position.character` is interpreted as a **UTF-16 code unit** offset. For ASCII text this is intuitive; for documents with emoji or CJK glyphs, compute `character` in UTF-16 code units (negotiate `general.positionEncodings` in `initialize` if you need UTF-8/UTF-32).

### `kakehashi/node`

Entry point: resolve a position to the smallest (deepest) node containing it.

```jsonc
// Request
{
  "textDocument": { "uri": "file:///foo.md" },
  "position":     { "line": 3, "character": 5 },
  "injection":    true     // optional, default: false — see below
}

// Response: NodeInfo | null
{ "id": "01HX...", "type": "fenced_code_block" }
```

Returns `null` when the position is outside the document, the document is empty, or the requested injection layer does not exist at that position.

**Cursor-at-end-of-file**: a cursor sitting just past the last byte (`character` at end-of-line on the final line) still resolves — it is contained by the nodes that end at the document's end. An empty document always returns `null`.

#### The `injection` parameter

For documents with language injection (e.g. Markdown → Python → regex), `injection` selects which language layer to resolve in. The cursor's injection stack is `[host, layer₁, layer₂, …, deepest]`:

| Value | Resolves to |
|---|---|
| `false` (default) or `0` | host layer (layer 0) — always succeeds |
| `true` | deepest layer at the cursor (saturating) — always succeeds |
| positive `n` (`n ≥ 1`) | exactly layer `n`; `null` if the stack is shallower |
| negative `n` (`n ≤ -1`) | `n`-th layer from the deepest; `null` if out of range |

Example — cursor inside a regex literal in a Python code block in Markdown:

| `injection` | Returns |
|---|---|
| `false` / `0` | Markdown `code_fence_content` |
| `1` | the Python node |
| `2` / `true` / `-1` | the regex node |
| `3` | `null` (stack only has layers 0–2) |

For the exhaustive boundary and layer-selection rules, see the [Node Reference Protocol ADR](architecture-decisions/node-reference-protocol.md).

### `kakehashi/node/parent`

Walk one step toward the root of the **same language tree**.

```jsonc
{ "textDocument": { "uri": "..." }, "id": "01HX..." }   →   NodeInfo | null
```

Returns `null` when the `id` is unknown (re-acquire) or already refers to a root node.

**Navigation stays within one tree.** `parent` on the root of an *injected* tree returns `null`, **not** the host node that contains the injection — crossing an injection boundary requires a fresh `kakehashi/node` call. This keeps host and injected trees logically separate, and is unambiguous even for same-language injection (markdown-in-markdown), where a host and injected node may share an identical span and type but have distinct IDs.

### `kakehashi/node/children`

List the immediate children of a node, in **document order**.

```jsonc
{ "textDocument": { "uri": "..." }, "id": "01HX..." }   →   NodeInfo[] | null
```

- Returns **both named and anonymous** children.
- A node that exists but has no children returns `[]` (not `null`).
- Returns `null` only when the `id` is unknown (re-acquire).

Need positions for each child? Call `kakehashi/node/text` per child for now (an N+1 round trip), or wait for a future bulk/range endpoint.

### `kakehashi/node/text`

Resolve a node's **current** text content.

```jsonc
// Request
{ "textDocument": { "uri": "..." }, "id": "01HX..." }

// Response
{ "text": "print(\"hello world\")" }   // reflects post-edit content
null                                    // id is not currently valid — re-acquire
```

The text is sliced from the current document using the node's adjusted byte range, kept consistent with edits the client has already observed.

### Example Flow

```
1. Client → kakehashi/node          { textDocument, position, injection: true }
   Server ← { id: "01HX-A", type: "call" }

2. … user edits; didChange fires …

3. Client → kakehashi/node/text      { textDocument, id: "01HX-A" }
   Server ← { text: "foo(updated_arg)" }      // ID survived, text reflects the edit

4. Client → kakehashi/node/parent    { textDocument, id: "01HX-A" }
   Server ← { id: "01HX-B", type: "expression_statement" }

5. Client → kakehashi/node/children  { textDocument, id: "01HX-B" }
   Server ← [ { id: "01HX-A", type: "call" } ]
```

### Neovim Example

Using Neovim's built-in LSP client (0.11+), resolve the node under the cursor and print its type:

```lua
local function kakehashi_node_under_cursor()
  local client = vim.lsp.get_clients({ name = "kakehashi", bufnr = 0 })[1]
  if not client then return end

  local params = vim.lsp.util.make_position_params(0, client.offset_encoding)
  params.injection = true -- resolve the deepest injected layer

  client:request("kakehashi/node", params, function(err, result)
    if err or not result then return end
    vim.notify(("node: %s (%s)"):format(result.type, result.id))

    -- Walk to the parent using the returned id
    client:request("kakehashi/node/parent", {
      textDocument = params.textDocument,
      id = result.id,
    }, function(_, parent)
      if parent then
        vim.notify(("parent: %s"):format(parent.type))
      end
    end, 0)
  end, 0)
end
```

Because IDs survive edits, you can stash `result.id` and later call `kakehashi/node/text` to read the (possibly edited) text without re-resolving from a position. Treat any `null` response as "re-acquire via `kakehashi/node`".

## `kakehashi/internal/effectiveConfiguration`

Returns the **merged, effective** configuration the server is currently using — after combining `initializationOptions`, user/project `kakehashi.toml` files, and built-in defaults. Useful for debugging "why is this language configured this way?".

```jsonc
// Request
{}

// Response
{
  "settings": { /* matches the kakehashi.toml schema */ }
}
```

The `settings` value mirrors the schema documented under [Configuration](README.md#configuration). The `internal/` segment marks this as a diagnostic/introspection endpoint rather than part of the node platform surface.

## Planned Additions

The following are specified in the [Node Reference Protocol ADR](architecture-decisions/node-reference-protocol.md) but **not yet implemented** — they are listed here so clients can plan for them, not because they are callable today:

| Planned | Shape | Mirrors Tree-sitter |
|---|---|---|
| `namedOnly` parameter on `kakehashi/node` | `{ …, namedOnly?: boolean }`, default `false` | `named_descendant_for_byte_range` |
| `kakehashi/node/namedChildren` | `{ textDocument, id } → NodeInfo[] \| null` | `named_children()` |

Together these let a client resolve and navigate **named-only** nodes (e.g. parity with Neovim's `vim.treesitter.get_node{ include_anonymous = false }`) without filtering anonymous nodes client-side. Until they ship, resolve with `kakehashi/node` and inspect `type`, or walk `parent`/`children` and skip anonymous nodes in the client.
