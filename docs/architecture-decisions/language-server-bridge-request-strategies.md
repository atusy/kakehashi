# Language Server Bridge Request Strategies

> The single-LS, per-method strategies defined here remain in effect. Multi-LS
> routing, aggregation, and initialization-window handling are covered by
> ls-bridge-message-ordering and
> ls-bridge-server-pool-coordination.

## Decision–Implementation Gap

10 of the 11 per-method strategies described below are implemented
(definition, hover, signatureHelp, completion, references, rename,
codeAction, formatting, documentHighlight, diagnostics). Bridged semantic
tokens remain unimplemented.

## Context

When bridging LSP requests for injection regions (see language-server-bridge), different LSP methods have different characteristics:

| Method | Latency Sensitivity | kakehashi Capability | Language Server Value |
|--------|---------------------|--------------------------|----------------------|
| Semantic Tokens | High (visual feedback) | Good (Tree-sitter highlights) | Better (type-aware) |
| Go-to-Definition | Medium | Local only (locals.scm) | Cross-file resolution |
| Completion | High (typing flow) | None | Full |
| Hover | Low | None | Full |
| Diagnostics | Low (background) | None | Full |

A single bridge strategy doesn't fit all methods. We need per-method strategies that balance latency, correctness, and user experience.

### Injection Isolation Constraint

**Critical insight**: Injection regions are isolated code fragments with no
relationship to OTHER VIRTUAL REGIONS — each is analyzed on its own. Real
files on disk (library sources, workspace files) remain valid targets. This
affects how we handle features that can return cross-file results.

```
┌─────────────────────────────────────────────────────────────────┐
│  Host Document: tutorial.md                                     │
│                                                                 │
│  ┌─────────────────────┐    ┌─────────────────────────────────┐ │
│  │ ```rust             │    │ External crate file             │ │
│  │ use serde::Serialize│    │ (serde/lib.rs)                  │ │
│  │                     │    │                                 │ │
│  │ #[derive(Serialize)]│    │ This file does NOT exist in     │ │
│  │ struct Foo { ... }  │    │ our virtual workspace!          │ │
│  │ ```                 │    │                                 │ │
│  └─────────────────────┘    └─────────────────────────────────┘ │
│         │                              ▲                        │
│         │  go-to-definition            │                        │
│         │  on "Serialize"              │                        │
│         └──────────────────────────────┘                        │
│                                                                 │
│  Result: Location in serde crate → KEPT (real file on disk);   │
│  only OTHER VIRTUAL-REGION locations are filtered out           │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

## Decision

**Implement different bridge strategies based on LSP method characteristics, with careful handling of cross-file results and edit operations.**

### Strategy 1: Parallel Fetch with Progressive Refinement

**Applies to**: `textDocument/semanticTokens/full`, `textDocument/semanticTokens/range`

```
                    ┌─────────────────────────────┐
 Request ──────────▶│      kakehashi          │
                    │  ┌─────────────────────┐    │
                    │  │ Tree-sitter tokens  │────│───▶ Immediate response
                    │  │ (local, fast)       │    │     (use if bridge slow)
                    │  └─────────────────────┘    │
                    │           ▼                 │
                    │  ┌─────────────────────┐    │
                    │  │ Bridge to server    │────│───▶ rust-analyzer
                    │  │ (async)             │    │
                    │  └─────────────────────┘    │
                    │           │                 │
                    │           ▼                 │
                    │  ┌─────────────────────┐    │
                    │  │ Merge results       │────│───▶ Final response
                    │  │ (prefer bridged)    │    │     (replaces initial)
                    │  └─────────────────────┘    │
                    └─────────────────────────────┘
```

**Behavior**:
1. Fetch Tree-sitter tokens and bridged tokens **in parallel**
2. If bridged response arrives first → use it directly
3. If Tree-sitter response arrives first → return it immediately as provisional response
4. When bridged response arrives → send updated tokens (via `textDocument/semanticTokens/full` refresh mechanism)

**Rationale**: Users see instant syntax highlighting from Tree-sitter while richer type-aware tokens arrive asynchronously.

### Strategy 2: Full Delegation with Response Filtering

**Applies to**: `textDocument/definition`, `textDocument/declaration`,
`textDocument/typeDefinition`, `textDocument/implementation`,
`textDocument/references`, `textDocument/hover`, `textDocument/signatureHelp`

```
Request (cursor in injection) ──▶ Forward to language server
                                         │
                                         ▼
                                  Filter response
                                  (translate same-region virtual URIs,
                                   keep real-file URIs untranslated, drop
                                   other virtual-region URIs)
                                         │
                                         ▼
                                  Translate positions
                                  (virtual → host — same-region
                                   targets only; real-file locations
                                   keep URI and range unchanged)
```

**Per-Method Details**:

#### textDocument/definition (and declaration / typeDefinition / implementation, via the shared goto transformer)

| Aspect | Handling |
|--------|----------|
| Input | Position (host → virtual translation) |
| Output | Location[] (a scalar downstream Location is normalized to a vector) or LocationLink[] (for link-capable clients) |
| Cross-file | Real-file targets preserved (incl. host-URI targets, not containment-checked); only locations addressed to OTHER regions' virtual URIs filtered |
| Position mapping | Range start/end: virtual → host. Known gap: a LocationLink whose target is a real/host URI passes through whole, leaving its `originSelectionRange` (a source-document range) in virtual coordinates |

#### textDocument/references

| Aspect | Handling |
|--------|----------|
| Input | Position + includeDeclaration flag |
| Output | Location[] |
| Cross-file | Real-file locations preserved (incl. host-URI, not containment-checked); only locations addressed to OTHER regions' virtual URIs filtered |
| Position mapping | Each location's range: virtual → host |

**Important**: References may return many locations. Real-file locations pass
through as-is; locations in OTHER injection regions of the host document are
filtered (their virtual URIs would be meaningless to the editor).

#### textDocument/hover

| Aspect | Handling |
|--------|----------|
| Input | Position |
| Output | Hover (contents + optional range) |
| Cross-file | N/A (single location response) |
| Position mapping | Range only (if present) |

Simplest delegation—no filtering needed, minimal translation.

#### textDocument/signatureHelp

| Aspect | Handling |
|--------|----------|
| Input | Position + trigger context |
| Output | SignatureHelp (signatures + active parameter) |
| Cross-file | N/A |
| Position mapping | None needed |

No position information in response—pass through directly.

### Strategy 3: Delegation with Edit Filtering

**Applies to**: `textDocument/completion`, `completionItem/resolve`,
`textDocument/rename`, `textDocument/codeAction`, `textDocument/formatting`,
`textDocument/rangeFormatting`, `textDocument/onTypeFormatting`,
`textDocument/inlayHint` (its `textEdits`), `textDocument/colorPresentation`

These methods return edits that must be carefully validated.

#### textDocument/completion

```
┌─────────────────────────────────────────────────────────────────┐
│                    Completion Response                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  CompletionItem {                                               │
│    label: "HashMap",                                            │
│    textEdit: { range: ..., newText: "HashMap" },  ──▶ TRANSLATE │
│    additionalTextEdits: [                                       │
│      { range: {0,0}-{0,0}, newText: "use std::...\n" }          │
│    ]  ─────────────────────────────────────────────▶ TRANSLATE  │
│  }                                                              │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

**additionalTextEdits Problem**:

When completing `HashMap`, rust-analyzer wants to add an import:
```rust
// additionalTextEdit wants to insert at line 0:
use std::collections::HashMap;

fn main() {
    let m = HashMap::new();  // ← completion here
}
```

But line 0 of the virtual document maps to the injection start line in the host—**inside the code fence**, not at the file top where imports belong.

**Implemented policy** (fail-closed, atomic):

- The primary `textEdit`/`InsertReplaceEdit` (or the insertText/label
  fallback, and snippet variables whose client-side expansion is unknowable)
  is validated against the injection region — containment, per-line prefix
  preservation, and the fence-boundary rule. An unsafe primary drops the
  whole item at completion time; at `completionItem/resolve` time the unsafe
  resolved response is discarded and the original (already-validated)
  unresolved item is served instead.
- `additionalTextEdits` are validated as one atomic set: any unsafe member
  drops the entire array (never a subset — the array can carry paired halves
  of one operation). The item is kept; its primary insertion stays
  mechanically applicable, though possibly semantically incomplete without
  its auto-import (availability over fidelity, with a warn log).
- The same guards apply to inlay-hint `textEdits` (the hint is kept, its
  accept-edit set drops whole) and color presentations (an unsafe explicit or
  implicit label-replacement edit drops the presentation; unsafe
  additionalTextEdits drop as an array).

#### textDocument/rename

| Aspect | Handling |
|--------|----------|
| Input | Position + newName |
| Output | WorkspaceEdit (changes across files) |
| Cross-file | Real-file edits preserved (shared WorkspaceEdit transform, incl. host-URI edits — not containment-checked on this path); edits keyed to OTHER regions' virtual URIs filtered |
| Position mapping | All TextEdit ranges |

Rename can affect multiple files. Real-file edits (a project-aware server's
cross-file rename) are preserved — content and ranges untouched, though
bridge-local `TextDocumentEdit.version` values are cleared before relaying —
while entries addressed to other regions' virtual URIs (meaningless to the
editor) are FILTERED out with usable siblings surviving. The whole result is
rejected only when an edit fails the region-safety guards or the edit carries
a file operation (create/rename/delete) targeting a virtual URI.

#### textDocument/codeAction

| Aspect | Handling |
|--------|----------|
| Input | Range + context (in-region diagnostics, translated) |
| Output | (CodeAction \| Command)[] — bare Commands are bridged too, their names encoded for executeCommand routing |
| Cross-file | Real-file edits and file operations are PRESERVED; rejected are: edits touching a foreign injection region, virtual-URI file operations, and host-document edits escaping the source region — surfaced as a disabled action for `disabledSupport` clients; without that capability, dropped in the initial response, and returned unresolved (re-enveloped) on `codeAction/resolve` |
| Position mapping | Action edit ranges (shared WorkspaceEdit transform: own virtual URI translated, real URIs pass unchanged, foreign virtual entries filtered/rejected) and diagnostics' primary ranges — diagnostic `relatedInformation` locations are NOT translated (known gap) |

#### textDocument/formatting / rangeFormatting / onTypeFormatting

| Aspect | Handling |
|--------|----------|
| Input | Options (or range for `textDocument/rangeFormatting`) |
| Output | TextEdit[] |
| Cross-file | N/A (single document) |
| Position mapping | All edit ranges |
| Multi-server | full formatting: `preferred` by default, `concatenated` opts into a sequential pipeline over `priorities` (also the membership allowlist — unlisted servers do not run). `textDocument/rangeFormatting`: `preferred` only |

Formatting responses are validated per edit (virtual-EOF bounds, region
containment, prefix preservation, fence-boundary rule) and dropped **whole**
when any edit is unsafe — a formatter answer is one atomic diff, so applying
only its safe edits could duplicate or lose content.
For multiple servers, the `concatenated`
behavior does **not** concatenate edit lists (that would overlap); it runs a
sequential formatter pipeline over `priorities`, which is both the
**membership allowlist** (servers not listed do not run) and the application order. This
applies to **full formatting only** — `textDocument/rangeFormatting` always uses `preferred`, even though it
shares the `textDocument/formatting` config key. See
concatenated-formatting-pipeline.

### Strategy 4: Background Collection

**Applies to**: `textDocument/publishDiagnostics`

Note: spontaneous pushes bypass the aggregation priorities/strategy machinery
on the proactive republish path — the diagnostic cache concatenates them
across servers, suppressing push slots from pull-capable servers in favor of
pull results while the pull layer is active (in mixed configurations this
can suppress a push whose server was not itself pulled). Cached pushes
answering a client pull via `pushFallback` fold in only push-driven servers'
slots, under cross-layer priorities/strategy only — server-level
`priorities`/`maxFanOut` are not reapplied. Configured
`_self` host-server pushes carry the real host URI and host-relative ranges,
so they are republished as-is (no URI filtering or translation step applies
to them).

```
                    ┌─────────────────────────────┐
 (No request)       │      kakehashi          │
                    │                             │
 Document Change ──▶│  Notify language servers    │
                    │           │                 │
                    │           ▼                 │
                    │  ┌─────────────────────┐    │
                    │  │ Collect diagnostics │◀───│──── rust-analyzer
                    │  │ from all servers    │    │
                    │  └─────────────────────┘    │
                    │           │                 │
                    │           ▼                 │
                    │  ┌─────────────────────┐    │
                    │  │ Filter by URI       │    │
                    │  │ Translate ranges    │────│───▶ publishDiagnostics
                    │  │ Merge (concatenate) │    │     to editor
                    │  └─────────────────────┘    │
                    └─────────────────────────────┘
```

**Behavior**:
- Language servers push diagnostics asynchronously
- VIRTUAL-region pushes: filtered to the virtual document URI; primary
  diagnostic ranges translated to host coordinates (`relatedInformation`:
  virtual-URI entries dropped, same-host entries translated, other real-file
  entries unchanged)
- `_self` HOST pushes: real host URI and host-relative ranges, republished
  as-is (no filtering or translation step)
- Concatenate diagnostics from multiple servers (multiplicity preserved — no
  deduplication)
- Forward combined diagnostics to the editor with host document URI

### Position Mapping Summary

| Response Type | Fields to Map |
|---------------|---------------|
| Location | uri (the request's own virtual URI rewrites to host; real/host URIs pass unchanged), range |
| Location[] | Each location |
| LocationLink[] | Links targeting the request's own virtual URI: target ranges + originSelectionRange translated; links targeting real/host URIs pass through WHOLE — originSelectionRange stays virtual (known gap) |
| Hover | range (if present) |
| CompletionItem | textEdit.range, additionalTextEdits[].range |
| TextEdit | range |
| WorkspaceEdit | The request's own virtual-URI entries (re-keyed + translated); real-URI entries pass unchanged; foreign virtual entries filtered or reject the edit |
| Diagnostic | range; relatedInformation[].location: virtual-URI entries dropped, same-host entries translated, other real files unchanged |
| CodeAction | Contained edits, same conditional rule |

### Multi-Server Merging Rules

When multiple servers are configured for a language:

| Method | Merging Strategy (as implemented; rows marked *aspirational* never shipped) |
|--------|------------------|
| Semantic Tokens | *Aspirational* — bridged semantic tokens are unimplemented |
| Go-to-Definition | `preferred`: first non-empty result (query in `priorities` order) |
| Find References | `preferred` by default (first non-empty); *concatenate + dedupe is aspirational* |
| Completion | `preferred` by default (first non-empty); *list merging is aspirational* |
| Hover | `preferred` by default (first non-empty); *content concatenation is aspirational* |
| Diagnostics | `concatenated` by default — multiplicity preserved, no dedup |
| Code Actions | `concatenated` by default: every server's actions in one menu, titles suffixed with "— {server}", no dedup of same-named actions (deliberate), at most one `isPreferred` kept |
| Formatting | `preferred` (first non-empty) by default; `concatenated` runs a sequential pipeline over `priorities` (which is also the membership allowlist — servers not listed do not run) — full formatting only. `textDocument/rangeFormatting` stays on `preferred` (concatenated-formatting-pipeline) |

`priorities` lists follow the ordered-allowlist semantics of
aggregation-priorities-wildcard: listed servers run in order, a `"*"` element
stands for the unlisted rest, and absence of the list means `["*"]`.

## Consequences

### Positive

- **Optimized UX per feature**: Each method gets the strategy that best fits its characteristics
- **Fast visual feedback**: Semantic tokens appear instantly via parallel fetch
- **Accurate navigation**: Go-to-definition uses authoritative language server
- **Safe editing**: Unrepresentable edits (foreign injection regions, virtual-URI file operations, region-escaping host edits) are rejected to prevent corruption; real cross-file edits pass through
- **Comprehensive diagnostics**: Aggregated from multiple sources

### Negative

- **Implementation complexity**: Four different strategies to implement and maintain
- **Feature limitations**: Some features degraded (auto-imports may be dropped when unsafe for the region)
- **Inconsistent latency**: Some features instant (semantic tokens), others have server latency
- **Refresh mechanism dependency**: Progressive refinement requires editor support for token refresh

### Neutral

- **Per-method configuration**: shipped — `bridge.<lang>.aggregation` overrides priorities/maxFanOut per LSP method, and strategy for the methods that consume it (diagnostics, code actions, full formatting; every other method dispatches `preferred` regardless — see aggregation-priorities-wildcard)
- **Server capability detection**: Some servers may not support all methods; need graceful degradation

## Implementation Status

The following table is a SELECTED-FEATURE summary of the bridged LSP methods
this ADR discusses in detail (the goto family rows stand for
definition/declaration/typeDefinition/implementation, which share one
transformer). The full, user-facing feature list — documentLink,
documentSymbol, prepareRename, documentColor, moniker, codeLens/resolve,
foldingRange, linkedEditingRange, … — lives in `docs/language-features.md`.

| Feature | Status | Notes |
|---------|--------|-------|
| definition (+ declaration / typeDefinition / implementation) | ✅ Implemented | Shared goto transformer; real-file URIs kept, cross-region virtual URIs dropped |
| hover | ✅ Implemented | Pass-through with position translation |
| signatureHelp | ✅ Implemented | Pass-through |
| completion | ✅ Implemented | Fail-closed edit guards; atomic additionalTextEdits drop |
| completionItem/resolve | ✅ Implemented | Envelope-routed; an unsafe resolved PRIMARY edit serves the unresolved item, unsafe additionalTextEdits drop as an atomic set |
| references | ✅ Implemented | Real-file URIs kept, cross-region virtual URIs dropped |
| rename | ✅ Implemented | With workspace edit validation |
| codeAction | ✅ Implemented | Edit-carrying, lazy (`codeAction/resolve` routed to the origin server), command-carrying (`workspace/executeCommand` name-routing + palette dispatch), host layer, multi-region menu merge; strict edit validation (cross-region / region bounds incl. per-line prefix floor) |
| formatting | ✅ Implemented | Whole-response atomic drop on unsafe edits |
| rangeFormatting | ✅ Implemented | Shares the formatting guards |
| onTypeFormatting | ✅ Implemented | Shares the formatting guards |
| inlayHint | ✅ Implemented | Unsafe accept-edit sets dropped whole; hint kept |
| colorPresentation | ✅ Implemented | Experimental opt-in; unsafe presentations dropped |
| documentHighlight | ✅ Implemented | Strategy-2 shape (single-document, position-mapped) |
| diagnostics | ✅ Implemented | Push + pull with host translation |
| semanticTokens | ❌ Not implemented | Would enable parallel fetch strategy |

### Original Implementation Priority

The original priority order (for reference):

| Priority | Feature | Complexity | User Value |
|----------|---------|------------|------------|
| 1 | hover | Low | High |
| 2 | signatureHelp | Low | High |
| 3 | completion | High | Very High |
| 4 | references | Medium | Medium |
| 5 | documentHighlight | Low | Medium |
| 6 | diagnostics | Medium | High |
| 7 | formatting | Medium | Medium |
| 8 | rename | High | Low (for injections) |
| 9 | codeAction | High | Medium |

## Related Decisions

- [language-server-bridge](language-server-bridge.md): Core LSP bridge architecture
- [language-server-bridge-virtual-document-model](language-server-bridge-virtual-document-model.md): How injections are represented as virtual documents
- [concatenated-formatting-pipeline](concatenated-formatting-pipeline.md): Multi-server formatting via a sequential pipeline (`strategy: "concatenated"`)
- [aggregation-priorities-wildcard](aggregation-priorities-wildcard.md): Ordered-allowlist semantics and the `"*"` element for `priorities` lists
- [cross-layer-aggregation](cross-layer-aggregation.md): How native/host/virt layer results combine, one level above the per-target strategies defined here
