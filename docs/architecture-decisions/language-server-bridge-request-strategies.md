# ADR-0008: Request-Specific Bridge Strategies

## Status

Accepted (Partially superseded by [ADR-0012](0012-multi-ls-async-bridge-architecture.md) for multi-LS aspects)

**Implementation**: 8 of 11 features implemented (definition, hover, signatureHelp, completion, references, rename, codeAction, formatting)

**Note**: Single-LS per-method strategies remain valid. See ADR-0012 for multi-LS routing, aggregation, and initialization window handling.

## Context

When bridging LSP requests for injection regions (see [ADR-0006](0006-language-server-bridge.md)), different LSP methods have different characteristics:

| Method | Latency Sensitivity | kakehashi Capability | Language Server Value |
|--------|---------------------|--------------------------|----------------------|
| Semantic Tokens | High (visual feedback) | Good (Tree-sitter highlights) | Better (type-aware) |
| Go-to-Definition | Medium | Local only (locals.scm) | Cross-file resolution |
| Completion | High (typing flow) | None | Full |
| Hover | Low | None | Full |
| Diagnostics | Low (background) | None | Full |

A single bridge strategy doesn't fit all methods. We need per-method strategies that balance latency, correctness, and user experience.

### Injection Isolation Constraint

**Critical insight**: Injection regions are isolated code fragments. They exist within a single host document and have no relationship to other files. This affects how we handle features that can return cross-file results.

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
│  Result: Location in serde crate → FILTER OUT (not in injection)│
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

**Applies to**: `textDocument/definition`, `textDocument/references`, `textDocument/hover`, `textDocument/signatureHelp`

```
Request (cursor in injection) ──▶ Forward to language server
                                         │
                                         ▼
                                  Filter response
                                  (remove cross-file locations)
                                         │
                                         ▼
                                  Translate positions
                                  (virtual → host)
```

**Per-Method Details**:

#### textDocument/definition (PoC implemented)

| Aspect | Handling |
|--------|----------|
| Input | Position (host → virtual translation) |
| Output | Location or Location[] |
| Cross-file | Filter out locations outside virtual document |
| Position mapping | Range start/end: virtual → host |

#### textDocument/references

| Aspect | Handling |
|--------|----------|
| Input | Position + includeDeclaration flag |
| Output | Location[] |
| Cross-file | **Filter out** locations outside virtual document |
| Position mapping | Each location's range: virtual → host |

**Important**: References may return many locations from external files. Only references within the same injection region are meaningful.

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

**Applies to**: `textDocument/completion`, `textDocument/rename`, `textDocument/codeAction`, `textDocument/formatting`

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
│    ]  ──────────────────────────────────────────────▶ VALIDATE  │
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

**Solutions**:
1. **Filter out**: Remove additionalTextEdits outside injection range (loses auto-import)
2. **Warn user**: Apply main edit, show message about skipped import
3. **Smart placement**: Detect import patterns and place at injection start (complex)

Recommended: Option 1 or 2 for initial implementation.

#### textDocument/rename

| Aspect | Handling |
|--------|----------|
| Input | Position + newName |
| Output | WorkspaceEdit (changes across files) |
| Cross-file | **Reject entirely** if any edit outside virtual document |
| Position mapping | All TextEdit ranges |

Rename can affect multiple files. For injections, only same-document renames are valid.

#### textDocument/codeAction

| Aspect | Handling |
|--------|----------|
| Input | Range + context (diagnostics) |
| Output | CodeAction[] (each may contain WorkspaceEdit) |
| Cross-file | Filter out actions with cross-file edits |
| Position mapping | All ranges in remaining actions |

#### textDocument/formatting / rangeFormatting

| Aspect | Handling |
|--------|----------|
| Input | Options (or range for rangeFormatting) |
| Output | TextEdit[] |
| Cross-file | N/A (single document) |
| Position mapping | All edit ranges |

Relatively simple—all edits are within the virtual document.

### Strategy 4: Background Collection

**Applies to**: `textDocument/publishDiagnostics`

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
                    │  │ Merge & dedupe      │    │     to editor
                    │  └─────────────────────┘    │
                    └─────────────────────────────┘
```

**Behavior**:
- Language servers push diagnostics asynchronously
- kakehashi filters to virtual document URI only
- Translate all diagnostic ranges to host coordinates
- Merge and deduplicate diagnostics from multiple servers
- Forward combined diagnostics to the editor with host document URI

### Position Mapping Summary

| Response Type | Fields to Map |
|---------------|---------------|
| Location | uri (rewrite to host), range |
| Location[] | Each location |
| Hover | range (if present) |
| CompletionItem | textEdit.range, additionalTextEdits[].range |
| TextEdit | range |
| WorkspaceEdit | All documentChanges/changes entries |
| Diagnostic | range, relatedInformation[].location |
| CodeAction | All contained edits |

### Multi-Server Merging Rules

When multiple servers are configured for a language:

| Method | Merging Strategy |
|--------|------------------|
| Semantic Tokens | Later server wins for overlapping ranges |
| Go-to-Definition | Return first non-empty result (query in order) |
| Find References | Concatenate all results, dedupe by location |
| Completion | Merge completion lists from all servers |
| Hover | Concatenate hover content with separator |
| Diagnostics | Merge all, dedupe by range + message |

## Consequences

### Positive

- **Optimized UX per feature**: Each method gets the strategy that best fits its characteristics
- **Fast visual feedback**: Semantic tokens appear instantly via parallel fetch
- **Accurate navigation**: Go-to-definition uses authoritative language server
- **Safe editing**: Cross-file edits are filtered to prevent corruption
- **Comprehensive diagnostics**: Aggregated from multiple sources

### Negative

- **Implementation complexity**: Four different strategies to implement and maintain
- **Feature limitations**: Some features degraded (no auto-import in completion)
- **Inconsistent latency**: Some features instant (semantic tokens), others have server latency
- **Refresh mechanism dependency**: Progressive refinement requires editor support for token refresh

### Neutral

- **Per-method configuration possible**: Future enhancement could allow users to override strategies
- **Server capability detection**: Some servers may not support all methods; need graceful degradation

## Implementation Status

The following table shows the current implementation status of bridged LSP methods:

| Feature | Status | Notes |
|---------|--------|-------|
| definition | ✅ Implemented | Full delegation with response filtering |
| hover | ✅ Implemented | Pass-through with position translation |
| signatureHelp | ✅ Implemented | Pass-through |
| completion | ✅ Implemented | With additionalTextEdits filtering |
| references | ✅ Implemented | With cross-file filtering |
| rename | ✅ Implemented | With workspace edit validation |
| codeAction | ✅ Implemented | With edit filtering |
| formatting | ✅ Implemented | With position mapping |
| documentHighlight | ❌ Not implemented | |
| diagnostics | ❌ Not implemented | Requires async push model |
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

- [ADR-0006](0006-language-server-bridge.md): Core LSP bridge architecture
- [ADR-0007](0007-language-server-bridge-virtual-document-model.md): How injections are represented as virtual documents
