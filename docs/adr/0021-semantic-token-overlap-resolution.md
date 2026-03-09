# ADR-0021: Semantic Token Overlap Resolution Strategy

| | |
|---|---|
| **Status** | Accepted (Phases 1-2 Implemented, basic multiline splitting added) |
| **Date** | 2026-02-14 |

## Context

kakehashi provides semantic tokens via the LSP `textDocument/semanticTokens` protocol. Tokens are collected from tree-sitter highlight queries on the host document (e.g., Markdown) and from injection languages (e.g., Lua inside a fenced code block). These tokens frequently overlap, and the LSP specification requires that semantic tokens be **non-overlapping** within the response — unless the client declares `overlappingTokenSupport: true` in its `SemanticTokensClientCapabilities` (LSP 3.16.0+).

### The Overlap Problem

Overlapping tokens arise from two distinct mechanisms:

#### 1. Same-language nesting (intra-injection)

A single highlight query can produce captures at different tree depths that overlap in the source text. For example, in Markdown:

```markdown
# Hello **world**
```

The query produces both `@markup.heading.1` on the `inline` node (covering "Hello **world**") and `@markup.bold` on the `strong_emphasis` node (covering "**world**"). These tokens overlap, and a correct LSP response must split the heading token around the bold token.

#### 2. Cross-injection overlap (host vs. injection)

The host language captures tokens on nodes that span injection boundaries. For example:

```markdown
~~~lua
local x = 1
~~~
```

The Markdown query may capture `@markup.raw.block` on the `fenced_code_block` node, which spans the entire block including fences and content. The Lua injection produces its own tokens for `local x = 1`. Without splitting, both sets of tokens overlap, producing undefined behavior in LSP clients.

### Previous Approach

The original implementation used two mechanisms:

1. **Deduplication at exact `(line, column)` positions**: When two tokens started at the same position, the one with higher priority (by injection depth, then pattern index) won and the other was discarded entirely.
2. **Collection-time exclusion**: Before collecting host tokens, injection regions were pre-computed and host captures whose nodes fell *strictly inside* an injection region were suppressed.

This approach had three limitations:

- **No splitting**: A parent token covering a larger range than a child token was either kept entirely or discarded entirely. There was no way to split it into fragments around the child.
- **Spanning node leak**: Host captures on nodes that *extend beyond* injection boundaries (e.g., `@markup.raw.block` on `fenced_code_block`) passed the strictly-contained check and appeared in output, creating overlapping tokens.
- **Inactive injection suppression**: Injection regions where the language was resolved but produced zero captures still suppressed host tokens, causing gaps in highlighting.

## Decision

Adopt a **two-stage overlap resolution** model in `finalize_tokens`:

1. **Injection region exclusion**: Remove host tokens (depth=0) that fall inside *active* injection regions.
2. **Sweep line splitting**: For remaining tokens, split overlapping tokens into non-overlapping fragments using a breakpoint-based sweep line algorithm.

### Unified Principle

> **Child elements "punch holes" in parent tokens. No child means the parent covers everything.**

| Scenario | Parent | Child | Result |
|----------|--------|-------|--------|
| `# foo **emph** bar` | heading (nd=1) | emphasis (nd=2) | heading on "# foo " and " bar" |
| `**bold [link](url)**` | bold (nd=2) | link_text (nd=3) | bold on "**bold " and "**" |
| ```` ```lua\nx=1\n``` ```` | markup.raw (nd=1) | Lua tokens (depth=1) | markup.raw excluded; Lua tokens only |
| ```` ```\nfoo\n``` ```` | markup.raw (nd=1) | none (unknown lang) | markup.raw everywhere |

### Architecture

```
Host tokens (depth=0)     Injection tokens (depth>=1)
        |                         |
        +------------+------------+
                     |
                     v
    +-----------------------------+
    |  Stage 1: Injection         |  Remove host tokens inside active
    |  Region Exclusion           |  injection regions
    +-----------------------------+
                     |
                     v
    +-----------------------------+
    |  Stage 2: Sweep Line        |  Split remaining overlaps using
    |  (per-line breakpoints)     |  priority (depth, node_depth, pattern_index)
    +-----------------------------+
                     |
                     v
    +-----------------------------+
    |  Delta Encoding             |  Convert to LSP delta-relative positions
    +-----------------------------+
```

### Stage 1: Injection Region Exclusion

Host tokens (depth=0) overlapping **active** injection regions are handled with four cases:

| Case | Condition | Action | Example |
|------|-----------|--------|---------|
| **Fully inside** | Token bounds ⊂ region bounds (strict) | Remove | Code block content line inside blockquote injection |
| **Exact match** | Token bounds = region bounds | Keep for sweep line | Fish `(comment) @comment` + `(comment) @injection.content` |
| **Partial overlap, single-line region** | Token extends beyond a single-line region | Keep entire token for sweep line | Heading `## foo **bar**` overlapping `(inline)` injection |
| **Partial overlap, multiline region** | Token extends beyond a multiline region | Split, keep outside fragments only | Blockquote `> ` prefix survives, content removed |

The single-line/multiline distinction for partial overlaps identifies the injection's nature:
- **Single-line regions** arise from direct child injections (e.g., `markdown_inline` within a heading). The host token should fill gaps where the injection produces no tokens (e.g., plain text in headings).
- **Multiline regions** arise from container injections (e.g., blockquote content). The host token should not leak into the container's content.

Key concepts:

- **Active region**: An injection region that produced at least one token (depth >= 1). If a language was resolved but the highlight query produced no captures, the region is *inactive* and host tokens are preserved.
- **Spanning nodes**: Host captures on nodes that *extend beyond* injection boundaries (e.g., `@markup.raw.block` on `fenced_code_block`) are excluded because they fall inside the active injection region.

The `InjectionRegion` struct carries line/column boundaries converted from byte ranges:

```rust
struct InjectionRegion {
    start_line: usize,
    start_col: usize,   // UTF-16
    end_line: usize,
    end_col: usize,      // UTF-16
}
```

Regions are computed in `collect_injection_tokens_parallel` and passed to `finalize_tokens`.

### Stage 2: Sweep Line Algorithm

For each line, the sweep line:

1. Collects all **breakpoints** (start and end columns of every token on the line)
2. Sorts and deduplicates breakpoints
3. For each interval `[bp[i], bp[i+1])`:
   - Finds all tokens covering the interval
   - Picks the **winner** by priority: `(depth DESC, node_depth DESC, pattern_index DESC)`
   - Emits a fragment with the winner's properties
4. Merges adjacent fragments with the same token type back into a single token

**Priority rationale**:

| Dimension | Meaning | Why DESC |
|-----------|---------|----------|
| `depth` | Injection nesting level (0=host) | Deeper injections are more specific |
| `node_depth` | Distance from CST root | Deeper nodes are more specific within the same query |
| `pattern_index` | Position in the query file | Later patterns are intentionally more specific overrides |

`node_depth` is computed by walking the tree-sitter `node.parent()` chain during token collection, enabling priority-based resolution without requiring tree-sitter nodes at finalize time.

### Processing Pipeline

```
semantic.rs::handle_semantic_tokens_full
  |
  +-- collect_host_tokens(exclusion_ranges=&[])
  |     |
  |     +-- Emits all host tokens (no premature exclusion)
  |
  +-- collect_injection_tokens_parallel
  |     |
  |     +-- collect_injection_contexts_sync (discover injections)
  |     |
  |     +-- process_injection_sync (per injection, Rayon parallel)
  |     |     |
  |     |     +-- collect_injection_contexts_sync (nested)
  |     |     +-- collect_host_tokens(exclusion_ranges=nested_ranges)
  |     |     +-- process_injection_sync (recursive, same thread)
  |     |
  |     +-- compute_active_injection_regions
  |           (byte ranges -> InjectionRegion, filtered by token presence)
  |
  +-- finalize_tokens(all_tokens, &active_injection_regions, &lines)
        |
        +-- split_multiline_tokens (normalize to single-line fragments)
        +-- retain(length > 0)
        +-- Stage 1: injection region exclusion
        +-- Stage 2: split_overlapping_tokens (sweep line)
        |     +-- merge_adjacent_fragments
        +-- Delta encoding -> SemanticTokensResult
```

Note: The `exclusion_ranges` parameter in `collect_host_tokens` is still used for **nested injection exclusion** within `process_injection_sync` -- it prevents a parent injection from emitting tokens in regions covered by its child injections. This is distinct from the host-level exclusion in Stage 1.

### Multiline Token Handling

The sweep line operates **per-line**. Before running the sweep line, `finalize_tokens` calls `split_multiline_tokens`, which normalizes any multiline tokens into single-line fragments. The sweep line then processes those fragments together with tokens that were already single-line.

- When `supports_multiline = false` (the common case), tokens are already split per-line during collection
- When `supports_multiline = true`, `split_multiline_tokens` ensures multiline tokens participate correctly in overlap resolution
- Advanced cross-line merging heuristics (e.g., re-joining fragments after splitting) are deferred to a future phase

## Consequences

### Positive

- **Correct non-overlapping output**: The LSP response contains no overlapping tokens, regardless of query complexity or injection nesting depth. This eliminates undefined behavior in editors.
- **Granular highlighting**: Parent tokens like headings are split around children like bold/italic, preserving both semantics simultaneously. Previously, one or the other was lost.
- **Inactive region preservation**: Unknown-language code blocks (e.g., `` ```unknown ``) correctly preserve the parent `@markup.raw.block` token, since no injection tokens suppress it.
- **Spanning node correctness**: Host captures on nodes spanning injection boundaries are correctly excluded, fixing the overlapping output bug.
- **Unified resolution point**: All overlap resolution happens in `finalize_tokens`, making the algorithm easy to reason about, test, and extend.
- **24 unit tests** cover sweep line splitting, injection exclusion, and edge cases.

### Negative

- **O(n*b) sweep line complexity**: For each line, the algorithm iterates all tokens on that line for each breakpoint interval. With many overlapping tokens on a single line, this could be slow. In practice, typical lines have fewer than 10 tokens, so this is not a concern.
- **Additional memory for `node_depth`**: Each `RawToken` carries a `node_depth` field (one `usize`), increasing per-token memory. The cost is negligible relative to the `mapped_name: String` already present.
- **Multiline splitting is basic**: `split_multiline_tokens` decomposes multiline tokens into per-line fragments, which is sufficient for overlap resolution. More advanced heuristics (e.g., cross-line fragment merging) are deferred.
- **Adjacent fragment merging adds a post-processing pass**: After splitting, adjacent fragments with the same type are merged. This is an O(n) pass on the already-split tokens.

### Neutral

- **Collection-time exclusion retained for nested injections**: The `is_in_exclusion_range` check in `collect_host_tokens` is still used within `process_injection_sync` for parent-child injection relationships. Only the host-level exclusion moved to finalize time.
- **Delta encoding unchanged**: The LSP delta-relative encoding step is unaffected by the new splitting logic.
- **`semanticTokens/range` automatically benefits**: The range handler delegates to `handle_semantic_tokens_full`, so the splitting algorithm applies to range requests without additional work.

## Alternatives Considered

### Alternative 1: Tree-sitter Priority Directives

Use tree-sitter's built-in `#set! priority` directives to resolve overlaps at query time.

**Not Chosen Because:**
- Only resolves conflicts *within* a single query, not across injection boundaries
- Requires modifying upstream query files for every language
- Cannot split tokens -- can only choose one winner per position
- No standard priority convention across language grammars

### Alternative 2: Layered Token Output

Return multiple token arrays (one per injection layer), letting the client merge them.

**Not Chosen Because:**
- LSP `textDocument/semanticTokens` returns a flat token array; there is no layered API
- Would require a non-standard protocol extension
- Pushes complexity to every editor client

### Alternative 3: Full Collection-Time Splitting

Split tokens during collection rather than during finalization, computing exact fragments as each capture is processed.

**Not Chosen Because:**
- Requires access to all tokens at collection time, conflicting with parallel processing
- Injection tokens arrive asynchronously from Rayon workers; cannot split host tokens until injections are complete
- Mixing splitting with collection makes both harder to test independently
- Finalize-time splitting cleanly separates concerns: collection gathers raw data, finalize resolves conflicts

### Alternative 4: Z-Index / Stacking Order

Assign each token a z-index and let the highest-z token "win" at each position, discarding lower layers entirely (no splitting).

**Not Chosen Because:**
- Loses parent token information on non-overlapping regions (e.g., heading text outside bold spans)
- Editors benefit from seeing heading tokens on the surrounding text for consistent coloring
- Splitting preserves full semantic information

## Deferred Phases

### Phase 3: Advanced Multiline Heuristics

Basic multiline splitting (`split_multiline_tokens`) is implemented: multiline tokens are decomposed into per-line fragments before the sweep line. Potential future improvements:

- Cross-line fragment merging after overlap resolution
- Smarter handling of tokens spanning empty lines
- Low priority: the current per-line approach handles all known use cases

### Phase 4: `overlappingTokenSupport` Pass-Through (Future ADR)

The LSP `SemanticTokensClientCapabilities` includes an `overlappingTokenSupport?: boolean` field ([LSP 3.16.0](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.18/specification/)). When a client declares `overlappingTokenSupport: true`, the server is permitted to return overlapping tokens — the client is responsible for layering them correctly.

Currently, kakehashi **always** performs the sweep line splitting (Stage 2) regardless of this capability. This is the conservative default: it guarantees correct rendering on all clients.

However, when a client supports overlapping tokens, the sweep line may be **counterproductive**:

- **Information loss from splitting**: The sweep line chooses a single winner per interval. On `# Hello **world**`, the heading token is split and the bold region only shows `@markup.bold` — not both heading *and* bold. A client that supports overlapping tokens could render both layers simultaneously (e.g., bold + heading color), producing richer highlighting.
- **Unnecessary computation**: The sweep line's breakpoint collection, interval scanning, and fragment merging are wasted work when the client can handle raw overlapping tokens natively.
- **Injection region exclusion still needed**: Even with `overlappingTokenSupport: true`, Stage 1 (injection region exclusion) remains necessary. Host tokens inside active injection regions are semantically wrong (they represent the host language's interpretation of injected content), not just visually overlapping.

A future ADR should evaluate:

1. **Whether to skip Stage 2** when `overlappingTokenSupport: true` — return unsplit overlapping tokens, letting the client layer them
2. **Whether Stage 1 should change** — should injection region exclusion become optional, or is it always semantically correct?
3. **Editor behavior survey** — how do VS Code, Neovim, Helix, and Zed actually render overlapping semantic tokens? Do they layer, or does the last token win?
4. **Interaction with `augmentsSyntaxTokens`** — when the client uses semantic tokens to augment syntax highlighting, do overlapping tokens compose correctly with the syntax layer?

Until this is evaluated, kakehashi always produces non-overlapping output, which is universally correct.
