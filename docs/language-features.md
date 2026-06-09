# Language Features

This document describes the editor features kakehashi provides, method by method,
following the [LSP 3.18
specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/).
For each feature it explains what you can expect in practice — where it works, and
how kakehashi combines results when several language servers are involved — beyond
what the specification itself prescribes.

For how to install language servers and configure bridging, see
[docs/README.md](README.md#bridge-configuration). This document describes
**what the features do**; the README describes **how to set them up**.

---

## How features are provided

kakehashi provides features in two ways.

**Built-in features** work for any language that has a Tree-sitter grammar, with no
extra setup:

- **Syntax highlighting** (semantic tokens)
- **Selection range** (expand/shrink selection by syntax structure)

**Bridged features** are everything else (hover, completion, go-to-definition,
diagnostics, …). kakehashi cannot compute these itself; instead it forwards your
request to a real language server that you configure for the embedded language. If
no server is configured, these features simply return nothing.

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
(see [docs/README.md](README.md#capturemappings)).

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
an embedded block. Combine strategy: `preferred`.

### Completion

[`textDocument/completion`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_completion)
and [`completionItem/resolve`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#completionItem_resolve)

Offers completions inside an embedded block (auto-triggered after `.` or `:`).
Additional details for a highlighted item (documentation, extra edits) are resolved
on demand from the server that produced it. Auto-import edits that would land
outside the embedded block are not applied, because the surrounding document is not
part of the snippet. Combine strategy: `preferred`.

### Signature help

[`textDocument/signatureHelp`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_signatureHelp)

Shows parameter hints while typing a call inside an embedded block (auto-triggered
after `(` or `,`). Combine strategy: `preferred`.

### Go to Definition / Declaration / Type Definition / Implementation

[`textDocument/definition`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_definition),
[`textDocument/declaration`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_declaration),
[`textDocument/typeDefinition`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_typeDefinition),
[`textDocument/implementation`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_implementation)

Jumps from a symbol in an embedded block to its definition. The target can be
**within the same block** or a **real file on disk** (e.g. a library dependency).
Targets that live in a *different* embedded block of the same document are not
offered, since blocks are independent snippets. Combine strategy: `preferred`.

### Find references

[`textDocument/references`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_references)

Lists references to the symbol under the cursor. Like go-to-definition, results are
limited to the same block and real files on disk; references in other embedded
blocks are not included. Combine strategy: `preferred`.

### Document highlight

[`textDocument/documentHighlight`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentHighlight)

Highlights other occurrences of the symbol under the cursor within its block.
Combine strategy: `preferred`.

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
snippet, renames are confined to that block. Combine strategy: `preferred`.

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
requested range. Combine strategy: `preferred`.

### Moniker

[`textDocument/moniker`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_moniker)

Returns monikers for the symbol under the cursor. Combine strategy: `preferred`.

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
directly — identify the node at a position, walk to its parent or children, and read
its current text — **without bundling Tree-sitter on the client side**.

A handle to a node (its `id`) stays valid across edits as long as the node survives,
so a plugin can hold a reference to "this function" and keep using it while the user
types.

> `kakehashi/internal/*` methods are not part of this public surface and are not
> documented here.

### `NodeInfo`

```typescript
type NodeInfo = {
  id: string;    // stable handle, valid across edits until the node changes
  type: string;  // tree-sitter node type, e.g. "fenced_code_block"
};
```

### Methods

Every method takes a `textDocument`. The `id`-based methods are tied to the document
they came from; querying with an `id` from a different document returns `null`.

| Method | Input | Output | Purpose |
|--------|-------|--------|---------|
| `kakehashi/node` | `{ textDocument, position, injection? }` | `NodeInfo \| null` | The smallest node at a position (on the chosen embedding layer) |
| `kakehashi/node/parent` | `{ textDocument, id }` | `NodeInfo \| null` | The node's parent (within the same language tree) |
| `kakehashi/node/children` | `{ textDocument, id }` | `NodeInfo[] \| null` | The node's immediate children, in document order |
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
| negative `n` | the `n`-th layer counting from the deepest; `null` if out of range |

Example — Markdown containing a Python block containing a regex. With the cursor
inside the regex: `0` resolves the Markdown node, `1` the Python node,
`2` / `true` / `-1` the regex node, and `3` returns `null` (only three layers
exist).

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

> `kakehashi/node/namedChildren` and a `namedOnly` option on `kakehashi/node`
> (named-only navigation) are planned but **not yet available**.

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
