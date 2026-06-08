# Language Features

This document describes the LSP methods kakehashi implements, organized like the
[LSP specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)
(this project follows **LSP 3.18**). For each method it records the parts that the
specification deliberately leaves to the server: how a request behaves on the
**host document** versus an **injection (virtual) document**, how results from
multiple downstream servers are aggregated (`preferred` vs `concatenated`), and how
positions/URIs are translated. Custom `kakehashi/*` methods are documented at the
end.

For configuration of bridge servers, aggregation `priorities`, `strategy`, and
`maxFanOut`, see [README.md](README.md#bridge-configuration). This document
describes *behavior*; the README describes *setup*.

---

## Core model: host documents, native answers, and virtual documents

Three concepts recur throughout this document.

**Host document.** The real file the editor opened (the URI the client sent).
kakehashi parses it with Tree-sitter.

**Native vs. bridged.** kakehashi answers a request in one of two ways:

- **Native** — computed directly from the Tree-sitter syntax tree, with no
  downstream language server involved. Only **semantic tokens** and **selection
  range** are native. They are always available, fast, and require no
  configuration.
- **Bridged** — forwarded to a downstream language server (configured under
  `languageServers`). Every other feature in this document is bridged.

**Virtual document (injection region).** An injection region — e.g. a `python`
fenced code block inside Markdown, or SQL inside a Rust string — is presented to a
downstream server as its own synthesized *virtual document*
(`kakehashi:///…#injection-N.py`). Each injection region of the same language gets
its **own** virtual document (isolated mode), so duplicate symbols across blocks do
not collide. One downstream server process serves all virtual documents of its
language.

Because bridged features operate on virtual documents, kakehashi must translate
coordinates: a request `Position`/`Range` is mapped **host → virtual** before
forwarding, and every `Range`/`uri` in the response is mapped **virtual → host**
before returning to the editor.

> **Host-language bridging (`_self`) is not active.** The configuration schema
> reserves a `_self` key for bridging the host language itself to a whole-document
> server (e.g. marksman for Markdown), but this is **not wired into request
> dispatch** today. In practice, **content outside injection regions receives only
> the native features** (semantic tokens, selection range). All bridged features
> below act *only* on injection regions.

### How a position-based bridged request flows

```
Editor request (host position)
        │
        ▼
Is the position inside an injection region?
        │ no ──────────────▶ return null  (no host bridge today)
        │ yes
        ▼
Pick the single region under the cursor
        │
        ▼
Translate host position → virtual position
        │
        ▼
Fan out to configured server(s) for that injection language
        │
        ▼
Aggregate responses (preferred / concatenated)
        │
        ▼
Translate response ranges/URIs virtual → host; filter cross-region/cross-file
        │
        ▼
Return to editor
```

Whole-document bridged requests (e.g. `documentSymbol`, `formatting`,
`diagnostic`) instead enumerate **all** injection regions, fan out one task per
region in parallel, translate each region's results, and concatenate them.

### Aggregation strategies

When more than one downstream server can serve an injection language, responses
are combined per method:

| Strategy | Behavior |
|----------|----------|
| `preferred` (default for everything except diagnostics) | Use the first **non-empty** response in `priorities` order; falls back to arrival order when no priorities configured. |
| `concatenated` (default for `textDocument/diagnostic` and `textDocument/publishDiagnostics`) | Collect responses from all servers and merge them. |

`maxFanOut` caps how many servers are queried. The per-method default is resolved
in `default_aggregation_strategy_for_method` (`src/config/settings.rs`). Note that
**formatting forcibly ignores `concatenated`** (see [Formatting](#formatting)) to
avoid producing overlapping edits.

---

## Server capabilities (advertised at `initialize`)

The capabilities below are declared in `src/lsp/lsp_impl/lifecycle.rs`. Anything
not listed here is **not** advertised (see [Not currently
provided](#not-currently-provided)).

| Capability | Value |
|------------|-------|
| `textDocumentSync` | Incremental; `openClose: true`; `save.includeText: false`; no `willSave` |
| `semanticTokensProvider` | `full` + `full/delta` (`delta: true`), `range: true`, with legend |
| `selectionRangeProvider` | `true` |
| `hoverProvider` | `true` |
| `completionProvider` | `resolveProvider: true`; trigger chars `.` `:` |
| `signatureHelpProvider` | trigger chars `(` `,`; retrigger `,` |
| `declarationProvider` / `definitionProvider` / `typeDefinitionProvider` / `implementationProvider` | `true` |
| `referencesProvider` | `true` |
| `documentHighlightProvider` | `true` |
| `documentSymbolProvider` | `true` |
| `documentLinkProvider` | `true` (no `resolveProvider`) |
| `renameProvider` | `true` with `prepareProvider: true` |
| `documentFormattingProvider` / `documentRangeFormattingProvider` | `true` |
| `inlayHintProvider` | `true` |
| `monikerProvider` | `true` |
| `diagnosticProvider` | pull diagnostics; `interFileDependencies: false`, `workspaceDiagnostics: false` |
| `colorProvider` | **only** under the `experimental` build feature |

**Position encoding.** kakehashi does **not** advertise a `positionEncoding`, so
per the spec the client must treat `Position.character` as **UTF-16 code units**.
The server converts to UTF-8 byte offsets internally. To use UTF-8/UTF-32, the
client must negotiate `general.positionEncodings` (not yet supported server-side).

---

## Native features (Tree-sitter, no bridge)

### Semantic Tokens

[`textDocument/semanticTokens/full`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_semanticTokens),
`textDocument/semanticTokens/full/delta`, `textDocument/semanticTokens/range`

- **Native.** Computed from `highlights.scm` Tree-sitter queries over the whole
  host document. No downstream server is involved.
- **Host + injections together.** Injection regions are highlighted natively via
  the injection-aware tokenizer — highlighting inside a fenced code block does
  **not** require a bridge server. This is the one feature where injected content
  is served without bridging.
- **Delta** (`full/delta`) is supported for incremental token updates.
- Token types/modifiers come from kakehashi's fixed `LEGEND_TYPES` /
  `LEGEND_MODIFIERS`; capture names are remapped via `captureMappings`
  configuration.

### Selection Range

[`textDocument/selectionRange`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_selectionRange)

- **Native.** Builds the expand/shrink hierarchy from the Tree-sitter AST,
  including nested injection structure, over the whole host document. No bridge.

---

## Bridged language features (injection regions only)

All methods in this section forward to downstream language servers running over
per-injection virtual documents. If the request is not inside an injection region
(or no server is configured for that injection language), the handler returns
`null`/empty — there is **no native fallback** (kakehashi does not answer
navigation from `locals.scm`).

### Hover

[`textDocument/hover`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_hover)

- Resolves the single injection region under the cursor; returns `null` if the
  position is not inside one.
- Position translated host → virtual; the response `range` (if any) translated
  virtual → host.
- Strategy: **preferred** (first non-empty).

### Completion

[`textDocument/completion`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_completion)
and [`completionItem/resolve`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#completionItem_resolve)

- Resolves the region under the cursor; trigger characters `.` and `:`.
- Each returned item is tagged with an internal envelope in `CompletionItem.data`
  identifying its origin server, so that `completionItem/resolve` is routed back to
  **that specific server** (no fan-out on resolve). If resolution fails, the
  original item is returned unchanged.
- `textEdit`/`additionalTextEdits` ranges are translated virtual → host. Auto-import
  edits that fall outside the injection region are unsafe (the top of a virtual
  document maps to inside the code fence, not the file top) — see the limitations in
  [request-strategies ADR](architecture-decisions/language-server-bridge-request-strategies.md).
- Strategy: **preferred** (first server with non-empty items).

### Signature Help

[`textDocument/signatureHelp`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_signatureHelp)

- Region under the cursor; trigger chars `(` `,`, retrigger `,`.
- Pass-through (no positions in the response to translate).
- Strategy: **preferred**.

### Go to Definition / Declaration / Type Definition / Implementation

[`textDocument/definition`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_definition),
[`declaration`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_declaration),
[`typeDefinition`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_typeDefinition),
[`implementation`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_implementation)

These four behave identically:

- Region under the cursor.
- **Result filtering** (`transform_goto_response_to_host`):
  - Locations pointing into the **same** virtual document are translated virtual →
    host (including `originSelectionRange` for `LocationLink`).
  - Locations pointing to **real files** on disk are passed through unchanged
    (jumping out of the injection into a real dependency is allowed).
  - Locations pointing into **other** injection regions' virtual documents are
    **dropped** (cross-region offsets are not safe to translate).
- `LocationLink` vs `Location` is chosen based on client capability.
- Strategy: **preferred**.

### Find References

[`textDocument/references`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_references)

- Region under the cursor; honors `includeDeclaration`.
- Same URI filtering as goto: own-region locations translated, real-file locations
  kept, other-region virtual locations dropped. An empty array is preserved (means
  "found nothing", distinct from a failure).
- Strategy: **preferred**.

### Document Highlight

[`textDocument/documentHighlight`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentHighlight)

- Region under the cursor; results are host-document-local, translated virtual →
  host.
- Strategy: **preferred**.

### Document Symbol

[`textDocument/documentSymbol`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentSymbol)

- **Whole-document**: enumerates **all** injection regions, fans out one task per
  region in parallel, and **concatenates** symbols across regions (each region's
  ranges translated virtual → host).
- Returns hierarchical `DocumentSymbol[]` or flat `SymbolInformation[]` based on
  the client's `hierarchicalDocumentSymbolSupport`.
- Strategy: **preferred** *per region*, then cross-region concatenation.

### Document Link

[`textDocument/documentLink`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentLink)

- **Whole-document**: all regions in parallel, links concatenated and translated
  virtual → host. No `documentLink/resolve` is advertised.
- Strategy: **preferred** per region, then concatenation.

### Rename / Prepare Rename

[`textDocument/rename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_rename)
and [`textDocument/prepareRename`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_prepareRename)

- Region under the cursor.
- `rename` returns a `WorkspaceEdit`; edit ranges are translated virtual → host.
  Because injections are isolated single-document regions, only same-region edits
  are valid.
- `prepareRename` is a pass-through for the region under the cursor.
- Strategy: **preferred**.

### Formatting

[`textDocument/formatting`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_formatting)

- **Whole-document**: all regions in parallel; the resulting `TextEdit`s (each over
  a disjoint region span) are translated virtual → host and **sorted by start
  position** before returning.
- An empty edit list from a server (`Some([])`) is treated as authoritative
  ("already formatted").
- **`concatenated` is forcibly ignored for formatting.** Concatenating independent
  edit lists across servers would produce overlapping/conflicting edits, so
  formatting always uses **preferred** per region. A warning is emitted at
  config-apply time if `concatenated` is configured for a formatting method. (The
  planned multi-server sequential formatting *pipeline* described in the
  [concatenated-formatting-pipeline ADR](architecture-decisions/concatenated-formatting-pipeline.md)
  is not yet implemented.)

### Range Formatting

[`textDocument/rangeFormatting`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_rangeFormatting)

- Like formatting, but each region's request is clipped to the region's byte range;
  regions disjoint from the requested range are skipped. Endpoints are clamped to
  content columns. Edits concatenated and sorted.
- Shares the `textDocument/formatting` aggregation config key but always uses
  **preferred**.

### Inlay Hint

[`textDocument/inlayHint`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_inlayHint)

- Range-based: resolves the region containing `range.start` and forwards the range;
  hints translated virtual → host.
- Strategy: **preferred**.

### Moniker

[`textDocument/moniker`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_moniker)

- Region under the cursor; pass-through.
- Strategy: **preferred**.

### Diagnostics

[`textDocument/diagnostic`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_diagnostic)
(pull) and
[`textDocument/publishDiagnostics`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_publishDiagnostics)
(push)

- **Whole-document**, **concatenated by default** — this is the one family whose
  default strategy is `concatenated` rather than `preferred`.
- All injection regions are processed in parallel; the strategy is resolved
  **per region**, so different injection languages can differ (e.g. a Python region
  may concatenate two servers while a Lua region prefers one).
- Pull diagnostics return a single `Full` report (`result_id: None`) merging all
  regions; a 5-second per-request timeout applies.
- Push diagnostics are collected and forwarded with the **host** URI; an empty
  vector signals "clear diagnostics". The server advertises pull diagnostics and
  forwards downstream `workspace/diagnostic/refresh` notifications upstream so the
  editor re-pulls.
- `interFileDependencies` and `workspaceDiagnostics` are both `false`.

### Document Color / Color Presentation (experimental)

[`textDocument/documentColor`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_documentColor)
and [`textDocument/colorPresentation`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#textDocument_colorPresentation)

- **Only available when built with the `experimental` feature** (`colorProvider` is
  otherwise not advertised).
- `documentColor`: whole-document, all regions in parallel, concatenated, returns
  `[]` rather than `null` when empty.
- `colorPresentation`: range-based, single region from `range.start`.
- Strategy: **preferred** (documentColor concatenates across regions after
  per-region preferred).

---

## Lifecycle and synchronization

| Method | Behavior |
|--------|----------|
| [`initialize`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#initialize) | Loads settings (initialization options merged over config files), stores client capabilities and workspace roots, forwards roots/folders/capabilities to the bridge pool, returns the capabilities above. |
| [`initialized`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#initialized) | Starts the loop that forwards downstream `workspace/diagnostic/refresh` to the editor. |
| [`shutdown`](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/#shutdown) | Persists crash-recovery state and gracefully shuts down all downstream servers. |
| `textDocument/didOpen` / `didChange` / `didClose` / `didSave` | Incremental sync. `didChange` re-parses and repositions tracked nodes; `didClose` drops the document and its node IDs. Changes are propagated to bridged virtual documents. |
| `workspace/didChangeConfiguration` | Re-applies settings. |

---

## `kakehashi/*` custom methods

kakehashi exposes a small set of syntax-tree primitives under the `kakehashi/`
namespace (see the
[node-reference-protocol ADR](architecture-decisions/node-reference-protocol.md)).
They let any LSP client query the Tree-sitter tree — identify a node at a position,
navigate parent/children, read its current text — **without bundling Tree-sitter on
the client**.

> `kakehashi/internal/*` methods are intentionally undocumented here — they are
> implementation-internal and not part of the supported surface.

### Node identity model

`kakehashi/node` returns a stable **`NodeInfo`**:

```typescript
type NodeInfo = {
  id: string;    // ULID, stable across edits as long as the node survives
  type: string;  // tree-sitter node type, e.g. "fenced_code_block"
};
```

- `type` (not `kind`) matches the tree-sitter web binding, the convention used by
  editor plugins.
- `range` and `named` are intentionally **not** included (kept orthogonal; may be
  added later via dedicated endpoints).
- The `id` survives `didChange` as long as the underlying node is not invalidated;
  it is dropped on `didClose`.

### Methods

All methods take a `TextDocumentIdentifier`. The id-based methods route by URI and
reject mismatched `(uri, id)` pairs as `null`.

| Method | Input | Output | Purpose |
|--------|-------|--------|---------|
| `kakehashi/node` | `{ textDocument, position, injection? }` | `NodeInfo \| null` | Smallest node containing `position` at the selected injection layer |
| `kakehashi/node/parent` | `{ textDocument, id }` | `NodeInfo \| null` | One step toward the root (within the same tree) |
| `kakehashi/node/children` | `{ textDocument, id }` | `NodeInfo[] \| null` | Immediate children (named **and** anonymous), in document order |
| `kakehashi/node/text` | `{ textDocument, id }` | `{ text: string } \| null` | Current text of the node, sliced from up-to-date content |

> **Implementation note.** The node-reference-protocol ADR also specifies a
> `namedOnly` parameter on `kakehashi/node` and a `kakehashi/node/namedChildren`
> method. These are **not implemented** yet — only the four methods above are
> registered (`src/bin/main.rs`), and `kakehashi/node` accepts only
> `textDocument`, `position`, and `injection`.

### The `injection` parameter (`boolean | number`, default `false`)

Selects which layer of the injection stack `[host, layer₁, …, deepest]` at the
cursor to resolve in:

| Value | Resolved layer |
|-------|----------------|
| absent / `false` / `0` | host (layer 0) — always succeeds |
| `true` | deepest layer (saturating) — always succeeds |
| positive `n` | exactly `stack[n]`; `null` if out of bounds |
| negative `n` | `stack[len + n]` (from the deepest); `null` if out of bounds |

Any other JSON shape (string, fractional number, explicit `null`) is rejected as
`null`. Example, in Markdown → Python → regex: at a cursor inside the regex,
`injection: 0` returns the Markdown node, `1` the Python node, `2`/`true`/`-1` the
regex node, `3` returns `null`.

### Boundary and null semantics

- **Half-open intervals** `[start, end)`: a cursor at byte `b` is inside `[s, e)`
  iff `s ≤ b < e`. A cursor exactly at a node's end byte is *outside* it — so a
  cursor on a closing code fence falls back into the host, not the injection.
- **End-of-document exception** (only when document length `L > 0`): a cursor at
  `b == L` is contained by nodes ending at `L`, so AST queries work at EOF. An empty
  document (`L == 0`) always returns `null`.
- **`null` is the universal "not currently resolvable" signal.** A client cannot
  distinguish "id invalidated by an edit", "id never issued", or "document not
  open" — all collapse to `null`. Treat `null` as "re-acquire via `kakehashi/node`".
- **Navigation stays within one language tree.** `parent` of an injected tree's
  root returns `null`, not the host node containing the injection — crossing
  injection boundaries requires a fresh `kakehashi/node` call.

---

## Not currently provided

These spec methods are **not** advertised in `ServerCapabilities` and are not
handled:

- `textDocument/codeAction` (and `codeAction/resolve`)
- `textDocument/codeLens`
- `textDocument/foldingRange`
- `textDocument/documentOnTypeFormatting`
- `textDocument/linkedEditingRange`
- `textDocument/callHierarchy` / `textDocument/typeHierarchy`
- `workspace/symbol`, `workspace/executeCommand`
- `documentLink/resolve`, `codeLens/resolve`
- Semantic tokens for ranges across multiple documents / workspace token refresh

Bridged features are additionally limited to **injection regions**: cross-region
and most cross-file navigation/edits are filtered out (only jumps to real files on
disk survive), and host-language content outside injections has no bridged support
until `_self` host bridging is wired into dispatch.
