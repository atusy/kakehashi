# Language Features

This document describes the editor features kakehashi provides, method by method,
following the
[LSP 3.18 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/).
For each feature it explains what you can expect in practice — where it works, and
how kakehashi combines results when several language servers are involved — beyond
what the specification itself prescribes.

For how to install language servers and configure bridging, see
[the configuration guide](README.md#bridge-configuration). This document describes
**what the features do**; the README describes **how to set them up**.

---

## How features are provided

kakehashi provides features in two ways.

**Built-in features** work for any language that has a Tree-sitter grammar, with no
extra setup:

- **Syntax highlighting** (semantic tokens)
- **Selection range** (expand/shrink selection by syntax structure)

**Bridged features** cover the rest of kakehashi's supported features (hover,
completion, go-to-definition, diagnostics, …) — not every LSP method
(see [Not currently provided](#not-currently-provided)). kakehashi cannot compute
these itself; instead it forwards your request to a real language server that you
configure for the embedded language. If no server is configured, these features
simply return nothing.

### Embedded code blocks

Bridged features work **inside embedded code blocks** — for example a `python`
fenced code block in Markdown, or SQL inside a string. kakehashi treats each
embedded block as a **standalone snippet** and hands it to the matching language
server. Two consequences follow from this, and they shape what you can expect:

- **Each block is analyzed on its own.** Two Python blocks can each define
  `main()` without conflicting. But features that need to see across blocks do not
  work between them — for example, you cannot go to a definition that lives in a
  *different* block.
- **Only embedded content is bridged.** The surrounding document itself (the
  Markdown prose, the host file as a whole) currently receives only the built-in
  features. Wiring a whole-document language server to the host file is not yet
  available.

When you trigger a bridged feature, kakehashi uses the embedded block under your
cursor (for position-based features like hover) or gathers results from **all**
embedded blocks (for whole-document features like document symbols and
diagnostics).

### When several servers handle one language

If you configure more than one language server for the same embedded language,
kakehashi combines their responses using one of two strategies, configurable per
method (`strategy` in the bridge configuration):

| Strategy | Behavior |
|----------|----------|
| `preferred` | Uses the first non-empty response, in your configured `priorities` order. **Default for every feature except diagnostics.** |
| `concatenated` | Merges the responses from all servers. **Default for diagnostics.** |

`maxFanOut` limits how many servers are queried. Formatting always uses `preferred`
regardless of configuration (merging independent formatting results would produce
conflicting edits).

---

## Built-in features

### Syntax highlighting

[`textDocument/semanticTokens`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_semanticTokens)
(full, delta, and range)

Highlights the whole document from Tree-sitter queries,
**including embedded code blocks** — you get highlighting inside fenced code blocks
even without a language server configured for that language. Delta updates and range
requests are supported. Highlight colors are driven by the token types/modifiers
kakehashi exposes; capture names can be remapped via `captureMappings`
(see [the configuration guide](README.md#capturemappings)).

### Selection range

[`textDocument/selectionRange`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_selectionRange)

Expands or shrinks the selection along the syntax tree, including the structure of
embedded blocks. Works for any grammar, no setup required.

---

## Bridged features

All features below require a language server configured for the embedded language.
Where the request must be inside an embedded code block, placing the cursor outside
one yields no result.

### Hover

[`textDocument/hover`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_hover)

Shows hover information for the symbol under the cursor, when the cursor is inside
an embedded block. Default combine strategy: `preferred`.

### Completion

[`textDocument/completion`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_completion)
and [`completionItem/resolve`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#completionItem_resolve)

Offers completions inside an embedded block (auto-triggered after `.` or `:`).
Additional details for a highlighted item (documentation, extra edits) are resolved
on demand from the server that produced it. Because the language server only sees
the isolated snippet, any edits it returns — including auto-import edits — are
placed relative to the embedded block, so file-level imports may not land where they
would in a standalone file. Default combine strategy: `preferred`.

### Signature help

[`textDocument/signatureHelp`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_signatureHelp)

Shows parameter hints while typing a call inside an embedded block (auto-triggered
after `(` or `,`). Default combine strategy: `preferred`.

### Go to Definition / Declaration / Type Definition / Implementation

[`textDocument/definition`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_definition),
[`textDocument/declaration`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_declaration),
[`textDocument/typeDefinition`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_typeDefinition),
[`textDocument/implementation`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_implementation)

Jumps from a symbol in an embedded block to its definition. The target can be
**within the same block** or a **real file on disk** (e.g. a library dependency).
Targets that live in a *different* embedded block of the same document are not
offered, since blocks are independent snippets. Default combine strategy: `preferred`.

### Find references

[`textDocument/references`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_references)

Lists references to the symbol under the cursor. Like go-to-definition, results are
limited to the same block and real files on disk; references in other embedded
blocks are not included. Default combine strategy: `preferred`.

### Document highlight

[`textDocument/documentHighlight`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentHighlight)

Highlights other occurrences of the symbol under the cursor within its block.
Default combine strategy: `preferred`.

### Document symbols

[`textDocument/documentSymbol`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentSymbol)

Lists symbols from **all** embedded blocks in the document, merged into one outline.
Returns a hierarchical or flat outline depending on what your editor supports.

### Document links

[`textDocument/documentLink`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentLink)

Collects clickable links from all embedded blocks.

### Rename / Prepare rename

[`textDocument/rename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_rename)
and [`textDocument/prepareRename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_prepareRename)

Renames a symbol within its embedded block. Because each block is a standalone
snippet, renames are confined to that block. Default combine strategy: `preferred`.

### Formatting

[`textDocument/formatting`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_formatting)

Formats every embedded block in the document. When multiple servers are configured,
the first one in `priorities` is used (formatting always uses `preferred`; merging
multiple formatters would conflict).

### Range formatting

[`textDocument/rangeFormatting`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_rangeFormatting)

Formats the embedded content overlapping the selected range. Behaves like
formatting, scoped to the selection.

### Inlay hints

[`textDocument/inlayHint`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlayHint)

Shows inline hints (types, parameter names) for the embedded block overlapping the
requested range. Default combine strategy: `preferred`.

### Moniker

[`textDocument/moniker`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_moniker)

Returns monikers for the symbol under the cursor. Default combine strategy: `preferred`.

### Diagnostics

[`textDocument/diagnostic`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_diagnostic)
(pull) and
[`textDocument/publishDiagnostics`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_publishDiagnostics)
(push)

Reports errors and warnings from every embedded block, merged into the document.
This is the one feature whose default combine strategy is **`concatenated`** — when
multiple servers are configured for a block, all of their diagnostics are shown.
The strategy is resolved per language, so different embedded languages can behave
differently.

### Document color (experimental)

[`textDocument/documentColor`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentColor)
and [`textDocument/colorPresentation`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_colorPresentation)

Shows color swatches and color picker presentations for embedded blocks.
**Only available in experimental builds** — standard builds do not advertise color
support.

---

## `kakehashi/*` methods

Beyond the standard LSP features, kakehashi exposes custom methods under the
`kakehashi/` namespace so that editor plugins and scripts can query the syntax tree
directly — identify the node at a position, walk it (parent, children, siblings,
descendants, fields), introspect it (kind, flags, byte/line-column spans), and read
its current text — **without bundling Tree-sitter on the client side**. These
mirror the Tree-sitter
[`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html) API.

A handle to a node (its `id`) stays valid across edits as long as the node survives,
so a plugin can hold a reference to "this function" and keep using it while the user
types.

> `kakehashi/internal/*` methods are not part of this public surface and are not
> documented here.

### `NodeInfo`

```typescript
type NodeInfo = {
  id: string;    // stable handle, valid across edits until the node changes
  kind: string;  // tree-sitter node kind, e.g. "fenced_code_block"
};
```

### Core methods

Every method takes a `textDocument`. The `id`-based methods are tied to the document
they came from; querying with an `id` from a different document returns `null`.

| Method | Input | Output | Purpose |
|--------|-------|--------|---------|
| `kakehashi/node` | `{ textDocument, position, injection? }` | `NodeInfo \| null` | The smallest node at a position (on the chosen embedding layer) |
| `kakehashi/node/parent` | `{ textDocument, id }` | `NodeInfo \| null` | The node's parent (within the same language tree) |
| `kakehashi/node/children` | `{ textDocument, id }` | `NodeInfo[] \| null` | The node's immediate children (named + anonymous), in document order |
| `kakehashi/node/namedChildren` | `{ textDocument, id }` | `NodeInfo[] \| null` | The node's immediate **named** children, in document order |
| `kakehashi/node/text` | `{ textDocument, id }` | `{ text: string } \| null` | The node's current text |

Positions use the LSP default encoding, UTF-16 code units. kakehashi does not
currently advertise or negotiate an alternate position encoding.

### The `injection` parameter

`kakehashi/node` accepts an optional `injection` (`boolean | number`, default
`false`) selecting which embedding layer to resolve the position in. Picture the
layers under the cursor as `[host, layer 1, layer 2, …, deepest]`:

| Value | Layer selected |
|-------|----------------|
| absent / `false` / `0` | the host document (always available) |
| `true` | the deepest embedded layer at the cursor |
| positive `n` | exactly layer `n`; `null` if there is no such layer |
| negative `n` | counts back from the deepest layer (`-1` = deepest, `-2` = next, …); `null` if out of range |

Example — Markdown containing a Python block containing a regex. With the cursor
inside the regex: `0` resolves the Markdown node, `1` the Python node,
`2` / `true` / `-1` the regex node, and `3` returns `null` (only three layers
exist).

### Node accessors

Once you hold a node `id`, these methods expose the rest of the Tree-sitter
[`Node`](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Node.html) API
one-for-one, so you can introspect and walk the tree without bundling Tree-sitter.
Every accessor takes `{ textDocument, id }` (plus the extra fields noted below);
any unresolvable `id` — or out-of-range argument — returns `null`.

**Introspection** — one property per call, returned as a single-field object
(e.g. `{ "kind": "fenced_code_block" }`):

| Method | Output |
|--------|--------|
| `kakehashi/node/kind` | `{ kind: string }` — the node's grammar symbol |
| `kakehashi/node/grammarName` | `{ grammarName: string }` — grammar name (differs from `kind` for aliased nodes) |
| `kakehashi/node/isNamed` · `isExtra` | `{ isNamed: bool }` · `{ isExtra: bool }` |
| `kakehashi/node/hasError` · `isError` · `isMissing` | `{ hasError: bool }` … (error-recovery flags) |
| `kakehashi/node/startByte` · `endByte` | `{ startByte: int }` · `{ endByte: int }` |
| `kakehashi/node/byteRange` | `{ startByte: int, endByte: int }` |
| `kakehashi/node/childCount` · `namedChildCount` · `descendantCount` | `{ childCount: int }` … |
| `kakehashi/node/toSexp` | `{ sexp: string }` — the subtree as an s-expression |

**Navigation** — returns `NodeInfo | null` (or `NodeInfo[] | null`), minted in the
same embedding layer as the input:

| Method | Extra input | Output |
|--------|-------------|--------|
| `kakehashi/node/child` · `namedChild` | `index` | `NodeInfo \| null` |
| `kakehashi/node/nextSibling` · `prevSibling` | — | `NodeInfo \| null` |
| `kakehashi/node/nextNamedSibling` · `prevNamedSibling` | — | `NodeInfo \| null` |
| `kakehashi/node/firstChildForByte` | `byte` | `NodeInfo \| null` |
| `kakehashi/node/descendantForByteRange` · `namedDescendantForByteRange` | `startByte`, `endByte` | `NodeInfo \| null` |

**Fields** — children labelled by the grammar (e.g. a function's `name` / `body`):

| Method | Extra input | Output |
|--------|-------------|--------|
| `kakehashi/node/childByFieldName` | `name` | `NodeInfo \| null` |
| `kakehashi/node/childrenByFieldName` | `name` | `NodeInfo[] \| null` |
| `kakehashi/node/fieldNameForChild` · `fieldNameForNamedChild` | `index` | `{ fieldName: string \| null } \| null` |

**Position / range** — line/column as LSP `Position` (`{ line, character }`, UTF-16):

| Method | Extra input | Output |
|--------|-------------|--------|
| `kakehashi/node/startPosition` · `endPosition` | — | `{ startPosition: Position }` · `{ endPosition: Position }` |
| `kakehashi/node/range` | — | `{ start: Position, end: Position }` |
| `kakehashi/node/descendantForPointRange` · `namedDescendantForPointRange` | `start`, `end` (Positions) | `NodeInfo \| null` |

> **Two coordinate systems.** Byte accessors (`startByte`, `byteRange`,
> `descendantForByteRange`, `firstChildForByte`, …) use **UTF-8 byte offsets** —
> Tree-sitter's native space, matching `kakehashi/node/text`. Position accessors use
> **LSP `Position` (UTF-16)**, matching `kakehashi/node` and every other LSP request.
> Pick whichever your client already works in; they address the same nodes.

### Result semantics

- **`null` means "not currently resolvable."** Whether the `id` was invalidated by
  an edit, never existed, or the document isn't open, the answer is the same. Treat
  `null` as a signal to re-acquire the node via `kakehashi/node`.
- **A cursor exactly at a node's end is outside it.** For example, a cursor on the
  closing fence of a code block resolves to the surrounding document, not the block.
- **A cursor at the very end of the document** still resolves (so syntax-aware
  commands work at end-of-file). An empty document returns `null`.
- **Navigation stays inside one language tree.** Asking for the parent of an
  embedded block's root returns `null`, not the host node containing it — to cross
  that boundary, call `kakehashi/node` again at the position.

> A `namedOnly` option on `kakehashi/node` (resolving the smallest *named* node at
> a position) is planned but **not yet available**. For named-only navigation today,
> use `kakehashi/node/namedChildren` and the `named*` accessors.

### Captures

The node accessors above walk the tree one step at a time. The
`kakehashi/captures/*` methods run a whole Tree-sitter
[query](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/) over the
document in a single request — so a client can do structural search, symbol
extraction, fold-range computation, or a
[treesitter-context](https://github.com/nvim-treesitter/nvim-treesitter-context)-style
"sticky context" feature without re-implementing the query engine.

The query itself is **not sent by the client**. A `kind` (e.g. `"context"`) names a
per-language query file, resolved as `queries/<language>/<kind>.scm` across the
configured `searchPaths` — the same place highlight queries live, with the same
`; inherits:` support. Drop a file into a search path and the kind exists; no
server configuration needed. (nvim-treesitter-context's own `context.scm` files
work as-is once their directory is on a search path.)

The three methods mirror `textDocument/semanticTokens`, so live features can
re-request cheaply on every cursor move or edit:

| Method | Input | Output |
|--------|-------|--------|
| `kakehashi/captures/full` | `{ textDocument, kind, matchLimit? }` | `CapturesResult \| null` |
| `kakehashi/captures/full/delta` | `{ textDocument, kind, previousResultId, matchLimit? }` | `CapturesResult \| CapturesDelta \| null` |
| `kakehashi/captures/range` | `{ textDocument, kind, range, matchLimit? }` | `{ matches, skipped } \| null` |

```typescript
type CapturesResult = {
  resultId: string;                  // hand back as previousResultId for a delta
  matches: Match[];
  skipped: {                         // patterns dropped by tolerant compilation
    startLine: number;               // 1-indexed line in the query file
    endLine: number;
    reason: string;                  // Tree-sitter's compile error for that pattern
  }[];
};

type Match = {
  patternIndex: number;              // which pattern in the query produced this match
  captures: {
    name: string;                    // capture name without the '@', e.g. "context"
    node: NodeInfo;                  // { id, kind } — trackable like any other node
    range: { start: Position, end: Position };  // LSP Position (UTF-16), inline
  }[];
};

type CapturesDelta = {
  resultId: string;
  // Splice edits over the previous matches array (indices are match indices):
  // matches.splice(start, deleteCount, ...data)
  edits: { start: number; deleteCount: number; data: Match[] }[];
};
```

- **`full` → `full/delta` loop**: call `full` once, keep its `resultId`, then send
  `full/delta` on subsequent ticks. An unchanged document answers with empty
  `edits`; a small edit ships only the changed matches. If the server doesn't
  recognize `previousResultId` (e.g. after a reopen), it falls back to a full
  result — clients need no special re-sync logic. Every response carries a fresh
  `resultId`; always hand back the latest one.
- **`range`** scopes the query walk to a viewport (matches whose nodes intersect
  the range). It carries no `resultId` — there is no delta over viewports.
- **Captures are grouped by match**, so correlated captures within one pattern
  (e.g. `@context` and `@context.end`) stay together.
- **Each capture carries both a `node` and its `range`**, so a bulk result needs no
  per-capture follow-up call. The `node.id` works with every accessor above.
- **Predicates are evaluated server-side** — the built-in `#eq?` / `#match?` /
  `#any-of?` and the Neovim-flavored `#lua-match?` / `#has-parent?` /
  `#has-ancestor?` (and their negations) — so results match kakehashi's own
  highlighting. Unknown predicates are ignored.
- **Tolerant compilation**: if some patterns reference symbols absent from the
  grammar, the valid patterns still run and the rest are reported in `skipped`.
- **`null` means "nothing here"**: the document isn't open, or the language has no
  `<kind>.scm` on the search paths. A malformed `kind` (anything beyond
  `[A-Za-z0-9_-]+`) is a client error, returned as JSON-RPC `InvalidParams`.

> Capture queries run against the **host layer only** for now; patterns are not yet
> matched inside embedded (injected) languages. Querying a Python block inside
> Markdown, for example, is planned but not yet available.

---

## Not currently provided

kakehashi does not yet provide these LSP features:

- Code actions / quick fixes (`textDocument/codeAction`)
- Code lens (`textDocument/codeLens`)
- Folding ranges (`textDocument/foldingRange`)
- On-type formatting (`textDocument/onTypeFormatting`)
- Linked editing (`textDocument/linkedEditingRange`)
- Call hierarchy / type hierarchy
- Workspace symbol search (`workspace/symbol`) and command execution
  (`workspace/executeCommand`)

Bridged features are also limited to **embedded code blocks**: navigation and edits
do not cross between blocks, and the surrounding host document has no bridged
language support yet (only built-in highlighting and selection range).
