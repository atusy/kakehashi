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
(see [Not currently provided](#not-currently-provided)). kakehashi mostly
forwards these requests to a real language server — one you configure for
the embedded language, or (for the surrounding document) a `bridge._self`
host server. Without a server they generally return nothing, though under
`KAKEHASHI_EXPERIMENTAL=true` a native Tree-sitter layer answers
definition/references/document highlight/rename lexically — for languages
that ship a `bindings.scm`, and not in regions shifted by runtime range
directives (`#offset!` / `#trim!`, e.g. frontmatter).

### Embedded code blocks

Bridged features work **inside embedded code blocks** — for example a `python`
fenced code block in Markdown, or SQL inside a string. kakehashi treats each
embedded block as a **standalone snippet** and hands it to the matching language
server. Two consequences follow from this, and they shape what you can expect:

- **Each block is analyzed on its own.** Two Python blocks can each define
  `main()` without conflicting. But features that need to see across blocks do
  not work between them — a definition target addressed through a *different*
  block's virtual document is filtered out (a server pointing at the host
  file's own URI can still land anywhere in it).
- **Embedded content is bridged per block; the host document is opt-in.** The
  surrounding document itself (the Markdown prose, the host file as a whole)
  receives the built-in features by default, and can additionally be bridged
  to the host language's own servers via `bridge._self`
  (host-document-bridge) — e.g. marksman answering on the Markdown file
  itself.

When you trigger a bridged feature, kakehashi uses the embedded block under your
cursor (for position-based features like hover) or gathers results from **all**
embedded blocks (for whole-document features like document symbols and
diagnostics).

### When several servers handle one language

If you configure more than one language server for the same embedded language,
kakehashi combines their responses using one of two strategies (`strategy` in
the bridge configuration). Only diagnostics, code actions, and full formatting
consume `concatenated`; every other method combines with `preferred`
regardless of the setting:

| Strategy | Behavior |
|----------|----------|
| `preferred` | Uses the first non-empty response, in your configured `priorities` order. **Default for every feature except diagnostics and code actions.** |
| `concatenated` | Merges the responses from all servers. **Default for diagnostics and code actions.** For full formatting it instead runs a sequential formatter pipeline over `priorities` (see [Formatting](#formatting)). |

This table describes aggregation between servers for one bridge target. Across
the `virt` / `host` / `native` layers, full semantic tokens are a deliberate
exception: every selected layer is overlaid in priority order, regardless of
the configured cross-layer `strategy`.

`priorities` is an ordered **allowlist**: servers absent from the list do not
run, and a `"*"` element stands for the unlisted rest (see the
[configuration reference](README.md) for details, including the `layers`
setting that orders result layers per method). `maxFanOut` limits how many
servers are queried.

---

## Built-in features

### Syntax highlighting

[`textDocument/semanticTokens`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_semanticTokens)
(full, delta, and range)

Highlights the whole document from Tree-sitter queries,
**including embedded code blocks** — you get highlighting inside fenced code blocks
even without a language server configured for that language. Delta updates and range
requests are supported. A range wholly contained in one embedded region is first
offered to that language's bridge server; outside such a region, an enabled
`bridge._self` server can answer before the built-in Tree-sitter fallback. Downstream
legends are remapped into kakehashi's advertised legend, and invalid or unknown
tokens are dropped.
For full-document requests, kakehashi first establishes the current built-in
Tree-sitter baseline, then fetches every selected host and virtual bridge layer.
Higher-priority bridge spans replace overlapping lower-priority spans while
uncovered built-in highlighting remains. A nested virtual region overlays its
containing virtual region; unrelated regions retain document order. Omitting
`native` from the layer priorities removes that built-in coverage. A bridge
target disabled by an empty server allowlist or zero fan-out does not break an
otherwise unchanged native full/delta lineage. The
`textDocument/semanticTokens/full/delta` request uses the same overlay: it
recomputes the current selected bridge layers, then returns their difference
from the previous merged full result.

Highlight colors are driven by the token types/modifiers
kakehashi exposes; capture names can be remapped via
`features.textDocument/semanticTokens.captureMappings`
(see [the configuration guide](README.md#featurestextdocumentsemantictokenscapturemappings)).

### Selection range

[`textDocument/selectionRange`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_selectionRange)

Expands or shrinks the selection along the syntax tree, including the structure of
embedded blocks. Works for any grammar, no setup required. When bridge layers are
configured, each requested position independently uses the first available
`virt`, `host`, or built-in `native` result in layer-priority order. Virtual
selection hierarchies are translated back into host-document coordinates.

---

## Bridged features

The features below are served by a language server configured for the
embedded language — also on the surrounding document itself where a
`bridge._self` host server is configured.
Placing the cursor outside an embedded
code block yields no result from the injection bridges; with `bridge._self`
configured, the host language's own servers still answer there.

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
would in a standalone file. Edits that would corrupt the host document around the
embedded block (escape the region, break blockquote `> ` prefixes, or merge content
into the closing fence) are dropped fail-closed: an unsafe primary edit drops
the item (at resolve time, the unsafe resolved response is discarded and the
unresolved item is served instead), while an unsafe auto-import set — at either
stage — is dropped whole and the completion itself still applies. Default combine strategy: `preferred`.

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
Targets that the server addresses to a *different* block's virtual URI are
not offered (blocks are independent snippets); host-URI targets are passed
through without a containment check. Default combine strategy: `preferred`.

### Find references

[`textDocument/references`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_references)

Lists references to the symbol under the cursor. Like go-to-definition,
results addressed to another block's virtual URI are not included; real-file
and host-URI results pass through (not containment-checked). Default combine
strategy: `preferred`.

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

Collects clickable links from all embedded blocks. Lazy links are resolved by
the same host or embedded-language server that produced them; links from an
edited or invalidated embedded region are returned unresolved instead of being
translated with stale coordinates.

### Rename / Prepare rename

[`textDocument/rename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_rename)
and [`textDocument/prepareRename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_prepareRename)

Renames a symbol within its embedded block. Edits that the server addresses
to OTHER regions' virtual URIs are filtered out; edits to real files (e.g. a
cross-file rename from a project-aware server, or host-URI edits — not
containment-checked) pass through. Default combine strategy: `preferred`.

### Formatting

[`textDocument/formatting`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_formatting)

Formats every embedded block in the document. When multiple servers are
configured, the default `preferred` strategy uses the first server in
`priorities` that returns edits. Setting `strategy = "concatenated"` on the
`textDocument/formatting` key instead runs a **sequential formatter pipeline**:
the servers named in `priorities` format the block one after another, each
seeing the previous formatter's output (e.g. `black` then `isort`). The
pipeline requires explicitly named servers — `"*"` is ignored there, since a
reproducible pipeline needs a deterministic order. It runs at most 64 named
servers per region; `maxFanOut` can set a lower limit.

A response whose edits would corrupt the host document around the embedded
block (escape the region, break blockquote `> ` prefixes, or merge content
into the closing fence) is dropped **whole** — a formatter answer is one
atomic diff, so applying only its safe edits could duplicate or lose content.
The same rule covers range and on-type formatting.

### Range formatting

[`textDocument/rangeFormatting`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_rangeFormatting)

Formats the embedded content overlapping the selected range. Scoped to the
selection, and always combined with `preferred` — the sequential
`concatenated` pipeline applies to full formatting only.

### Inlay hints

[`textDocument/inlayHint`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlayHint)

Shows inline hints (types, parameter names) for the embedded block overlapping the
requested range. A hint whose accept-time text edits would corrupt the host
document around the embedded block is served without them (the edits drop as
one atomic set). Label-part commands are re-keyed so `workspace/executeCommand`
routes them back to the producing server (the re-keyed names are not
registered, so a client that only dispatches registered command ids shows the
part without running its command, as for bridged code actions), and a label
part whose location points into another embedded block keeps its text but
loses the location.
Lazy hints can be completed through `inlayHint/resolve`; the
request returns to the exact downstream process that produced the hint. A hint
remains unresolved after any host-content revision (including same-size edits),
region movement or invalidation observed before the request is forwarded,
document reopen, or downstream replacement.
Resolve is also rejected for non-contiguous injections, where lazy edits could
cross masked gaps. Runtime query offsets such as frontmatter `#offset!` are
included in the freshness and coordinate checks. Default combine strategy:
`preferred`.

### Inline values

[`textDocument/inlineValue`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlineValue)

Shows values supplied by a debugger-aware language server for the embedded block
overlapping the requested range. Kakehashi translates both the visible range and
the debugger's stopped location into the block, then maps every returned text,
variable-lookup, or evaluatable-expression range back to the host document.
Virtual results whose ranges escape the block are dropped. A stopped location
outside the selected block does not have a valid virtual coordinate, so the
virtual layer is skipped and an enabled `bridge._self` host server may answer.
Downstream `workspace/inlineValue/refresh` requests are not forwarded; editors
refresh inline values only through their own debugger/client lifecycle.
Default combine strategy: `preferred`.

### Moniker

[`textDocument/moniker`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_moniker)

Returns monikers for the symbol under the cursor. Default combine strategy: `preferred`.

### Diagnostics

[`textDocument/diagnostic`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_diagnostic)
(pull) and
[`textDocument/publishDiagnostics`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_publishDiagnostics)
(push)

Reports errors and warnings from every embedded block, merged into the document.
The default combine strategy here is **`concatenated`** (shared with code
actions) — when multiple servers are configured for a block, all of their
diagnostics are shown. The strategy is resolved per language, so different
embedded languages can behave differently. Spontaneous pushes a server sends
on its own bypass the strategy machinery when proactively republished: they
are cached and concatenated across servers — except that whenever the pull
layer is active for the document, cached push slots from pull-capable
servers are suppressed in favor of pull results (no double-counting; in
mixed configurations this can suppress a push whose server was not itself
pulled). When those cached pushes later answer a client PULL
(`pushFallback`), only push-driven servers' slots fold in, under the
cross-layer priorities/strategy only — server-level `priorities`/
`maxFanOut` are not reapplied.

### Code actions

[`textDocument/codeAction`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_codeAction),
[`codeAction/resolve`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#codeAction_resolve),
[`workspace/executeCommand`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#workspace_executeCommand),
and [`workspace/applyEdit`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#workspace_applyEdit) (relay)

Quick fixes and refactors from the servers bridging the block (all of them
by default; `priorities`/`maxFanOut` can restrict the set) — a range
spanning several blocks merges each region's actions into one menu. Titles
are suffixed with "— {server}", exposing provenance and distinguishing
same-titled actions from *different* servers (identically titled actions
from one server — e.g. across two fences — keep identical titles).
Lazy actions resolve back to their origin server (`codeAction/resolve`) —
client-driven resolve routing additionally requires the client to declare
`dataSupport` and `resolveSupport` covering `"edit"`; without those,
injection-layer lazy actions are eagerly resolved by the bridge and
host-layer ones are disabled or dropped. (Known limitation: CLIENT-driven
resolve of lazy actions in runtime-range-adjusted regions (`#offset!` /
`#trim!`) such as bundled
YAML/TOML frontmatter always fails soft — the resolve freshness check cannot
match there; the eager-resolve path taken for non-envelope clients bypasses
that check.)
Command-carrying actions execute through the bridged
`workspace/executeCommand`.

Edit safety differs by layer and direction. An INJECTION-layer action edit
that cannot be represented in the host document (touching another injection
region, virtual-document file operations, or escaping the block) is
rejected: in the initial response it surfaces as a disabled action where the
client declares `disabledSupport` and is dropped otherwise; during `codeAction/resolve`
(where a response cannot be dropped) the unsafe payload is removed and the
action comes back disabled — or, for clients without `disabledSupport`,
unresolved. HOST-layer (`bridge._self`) action edits already target the
real document and pass through as-is. A downstream server's own
`workspace/applyEdit` request has its own policy built on the same
underlying transform: virtual-document edits are translated (real-file-only
requests pass through verbatim), and an untranslatable one — e.g. an unknown
or stale region, a multi-region edit, a virtual-URI file operation, or a
translated edit escaping the region or its per-line prefix floor — is
answered `applied: false` locally; it never reaches the editor and no action
is involved.

Default combine strategy: `concatenated`, across servers and across the
virt/host layers. Advertised only to clients with
`codeActionLiteralSupport`; see the README's bridged-requests list for the
palette/registered-list caveats.

### Call hierarchy

[`textDocument/prepareCallHierarchy`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_prepareCallHierarchy),
[`callHierarchy/incomingCalls`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#callHierarchy_incomingCalls),
and [`callHierarchy/outgoingCalls`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#callHierarchy_outgoingCalls)

Call hierarchy works for both embedded virtual documents and host-language
servers enabled with `bridge._self`. Prepared items remember the exact
downstream process that produced them, so incoming and outgoing expansion do
not silently switch to a replacement server. Virtual item ranges and call-site
ranges are translated back to host coordinates; real external-file items stay
in their original coordinate space. Results that point at another virtual
region are filtered because their coordinates cannot be represented safely in
the requesting host region.

Expansion fails softly with `null` when an item has no kakehashi routing data,
the host document changed or reopened, the embedded region moved or became
non-contiguous, or the producing downstream connection was replaced. Returned
callers and callees carry fresh routing data so clients can expand the tree
recursively.

### Type hierarchy

Type hierarchy preparation and both expansion directions are bridged for
embedded virtual documents and host-language bridge layers. Returned items
remember their exact producing server and region, so recursive supertype and
subtype requests return to that producer. Stale items from changed, reopened,
moved, or reconfigured documents fail softly with `null`.
Only items from the request's own virtual region are projected into host
coordinates; items from another known virtual region are filtered, while real
external-file URIs and their coordinate spaces are preserved.

### Document color (experimental)

[`textDocument/documentColor`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentColor)
and [`textDocument/colorPresentation`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_colorPresentation)

Shows color swatches and color picker presentations for embedded blocks.
Document colors can also combine embedded and host-language server results;
set `layers.aggregation."textDocument/documentColor".strategy = "concatenated"`
to retain both layers. Presentation routing is range-based because the LSP
color item carries no producer identity: outside an injection the host server
receives the real URI, range, and edits unchanged; inside an injection the
default `virt → host` preference can select the virtual server even for a
host-produced color. Configure `textDocument/colorPresentation` as
`concatenated` to retain answers from both layers in that ambiguous case.
Presentations whose primary edit (explicit, or the implicit label replacement)
would corrupt the host document around the embedded block are dropped
fail-closed; unsafe additional edits are dropped as one atomic set while the
presentation itself survives.
**Only available with the `KAKEHASHI_EXPERIMENTAL=true` environment variable** —
without the opt-in the server does not advertise color support.

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

Positions always use UTF-16 code units. When the client advertises
`general.positionEncodings`, kakehashi explicitly selects `utf-16`; otherwise
both sides use the LSP default of UTF-16.

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

Capture names in these responses are the raw query capture names with `@`
removed. `features.textDocument/semanticTokens.captureMappings` only controls
semantic-token presentation; it never renames or filters
`kakehashi/captures/*` results.

The three methods mirror `textDocument/semanticTokens`, so live features can
re-request cheaply on every cursor move or edit:

| Method | Input | Output |
|--------|-------|--------|
| `kakehashi/captures/full` | `{ textDocument, kind, injection? }` | `CapturesResult \| null` |
| `kakehashi/captures/full/delta` | `{ textDocument, kind, previousResultId }` | `CapturesResult \| CapturesDelta \| null` |
| `kakehashi/captures/range` | `{ textDocument, kind, range, injection? }` | `{ matches, skipped } \| null` |

```typescript
type CapturesResult = {
  resultId: string;                  // hand back as previousResultId for a delta
  matches: Match[];
  skipped: {                         // patterns dropped by tolerant compilation
    language: string;                // which language's kind file had the problem
    startLine: number;               // 1-indexed; in the query file — or in the
    endLine: number;                 // combined query when `; inherits:` is used
    reason: string;                  // Tree-sitter's compile error for that pattern
  }[];
};

type Match = {
  patternIndex: number;              // pattern within that language's kind query —
                                     // (language, patternIndex) is the unique key
  language: string;                  // language of the layer this match came from
  metadata?: Metadata;               // match-level (#set! key value); absent when none
  captures: {
    name: string;                    // capture name without the '@', e.g. "context"
    node: NodeInfo;                  // { id, kind } — trackable like any other node
    range: { start: Position, end: Position };  // LSP Position (UTF-16), inline
    metadata?: Metadata;             // capture-level (#set! @cap key value); absent when none
  }[];
};

// Values from #set! directives: strings as written in the query; the bare
// flag form (#set! key) surfaces as true.
type Metadata = { [key: string]: string | true };

type CapturesDelta = {
  resultId: string;
  // Splice edits over the previous matches array (indices are match indices):
  // matches.splice(start, deleteCount, ...data)
  edits: { start: number; deleteCount: number; data: Match[] }[];
};
```

- **The `injection` parameter** (`boolean`, default `false`): `false` runs the kind
  query on the host document only; `true` runs it across **every** embedded layer
  too — the host first, then each injected region in document order, nested
  injections included. Each layer resolves **its own language's**
  `queries/<lang>/<kind>.scm`, so a Markdown document with a Python block yields
  the Markdown heading *and* the Python function in one response; a language
  without the kind file simply contributes nothing. `language` is present on every
  match either way, and captured node ids resolve in their own layer when handed
  to `kakehashi/node/*`.
- **`full` → `full/delta` loop**: call `full` once, keep its `resultId`, then send
  `full/delta` on subsequent ticks. The delta carries **no `injection` parameter**
  — your `previousResultId` identifies the lineage and with it the mode; switch
  modes by issuing a new `full`. Lineages are kept **per mode**, so a host-only
  client and an injection client on the same document don't disturb each other.
  An unchanged document answers with empty `edits`; a small edit ships only the
  changed matches. A stale `previousResultId` falls back to a full result when
  the mode is unambiguous (only one mode in use); when it isn't — or the server
  has no lineage at all (never `full`ed, closed, restarted) — the answer is
  `null`: on `null`, call `full` again. Every response carries a fresh
  `resultId`; always hand back the latest one.
- **`range`** scopes the query walk to a viewport (matches whose nodes intersect
  the range); with `injection: true`, embedded layers outside the range are
  skipped entirely. It carries no `resultId` — there is no delta over viewports.
- **Captures are grouped by match**, so correlated captures within one pattern
  (e.g. `@context` and `@context.end`) stay together.
- **Each capture carries both a `node` and its `range`**, so a bulk result needs no
  per-capture follow-up call. The `node.id` works with every accessor above.
  `#offset!` and `#trim!` adjust the returned `range` with Neovim semantics
  without changing the raw node used by that id; a valid trim range takes
  precedence over an offset.
  On `@injection.content`, the same directives define the effective parser,
  routing, semantic, selection, and node-layer range. They also compose with
  `injection.combined`: each capture is adjusted before the ranges are grouped.
- **Predicates are evaluated server-side** — the built-in `#eq?` / `#match?` /
  `#any-of?` and the Neovim-flavored `#lua-match?` / `#has-parent?` /
  `#has-ancestor?` (and their negations) — with Neovim's `iter_matches`
  semantics: one failing predicate discards the **whole match**, so a guard
  capture filters the matches it guards rather than just disappearing from
  them. Unknown predicates are ignored.
- **`#set!` metadata is returned** (the
  [Neovim `set!` directive](https://neovim.io/doc/user/treesitter.html#treesitter-directive-set!)):
  `(#set! key value)` rides on the match as `metadata[key]`,
  `(#set! @cap key value)` rides on that capture as its `metadata[key]` —
  e.g. `((codeblock) @context (#set! kind "block"))` lets a client label
  matches without parsing capture names. Repeated keys are last-write-wins.
- **`#gsub!` transforms dynamic injection-language captures** before language
  normalization. `#offset!` and `#trim!` also adjust a dynamic language's text
  until `#gsub!` materializes it; later range directives leave that text intact.
  Multiple transformations run in query order using the same Lua-pattern
  subset as `#lua-match?`; unsupported `%b`, `%f`, position
  captures `()`, and pattern capture-backreference forms fail soft and leave
  the prior text unchanged.
  Composition with `#set! @capture text ...` is not supported because the Rust
  tree-sitter API does not retain its source order relative to general
  directives. The captures protocol itself has no transformed-text field, so
  `#gsub!` does not add capture metadata to these responses.
- **Tolerant compilation**: if some patterns reference symbols absent from the
  grammar, the valid patterns still run and the rest are reported in `skipped`.
- **`null` means "nothing here"**: the document isn't open, or no involved
  language has a `<kind>.scm` on the search paths (with `injection: true`, it's
  enough for *any* layer's language to have one). A malformed `kind` (anything
  beyond `[A-Za-z0-9_-]+`) is a client error, returned as JSON-RPC
  `InvalidParams`.

## Workspace symbols

`workspace/symbol` searches every configured, runnable language server in the
client workspace, including servers that have not yet been started by opening a
document. Results from all capable servers are combined in stable server-name
order. Legacy `SymbolInformation` results are normalized to `WorkspaceSymbol`.
For a `preferSharedInstance` server that cannot follow workspace-folder
changes, searches use a client-root fallback instead of a shared process that
may have been initialized from one document's marker root.

When a downstream server supports `workspaceSymbol/resolve`, returned symbols
carry opaque routing data so a later resolve reaches the same downstream
process, provided the editor's
`workspace.symbol.resolveSupport.properties` contains `location.range`. The
server's original `data` is preserved inside that envelope. If the producing
process has been replaced or its configuration was removed, resolve fails soft
by returning the unresolved symbol unchanged.

Partial-result and work-done tokens are not forwarded because one client token
cannot identify several downstream producers; the client receives one final
aggregate response.

---

## Current limitations

(The static code-action and execute-command providers are advertised only to
clients with `codeActionLiteralSupport`. Palette command names are registered
dynamically when the client declares
`workspace.executeCommand.dynamicRegistration` — covering the commands a
downstream server advertised in its initialize result once it reaches Ready;
commands a downstream registers dynamically afterwards are not routed.)

Bridged features are also limited to **embedded code blocks** in one respect:
navigation and edits do not cross between blocks — on the
goto/references/rename transforms, results addressed to another block's
virtual URI are filtered out (document-link targets are the exception and
pass through untouched), and a code action touching another region stays
visible as a disabled entry for `disabledSupport` clients (without that
capability: dropped from the initial response, returned unresolved on
`codeAction/resolve`). One `applyEdit` nuance: the translator picks its
target region from the edit itself, so a request touching exactly one live
virtual URI is translated against that region even if it differs from the
block that prompted the server's request (edits spanning multiple virtual
URIs are rejected on cardinality grounds; single-target requests can still
be rejected on the usual grounds — unknown/stale URI, virtual-URI file
operation, region escape). Real-file and — for navigation/references/rename — host-URI
results pass through, while injection-layer code actions (and applyEdit
requests that also touch a virtual document) constrain host-URI edits to
the region.
The surrounding host document can be bridged to the host language's own
servers via `bridge._self` (host-document-bridge).
