# Injection Token Cache Reuse

<!--
Status: Proposed (aspirational). The invalidation half described here is
implemented; the reuse half is not. See "Decision–Implementation Gap".
-->

**Related Decisions**: [semantic-token-overlap-resolution](semantic-token-overlap-resolution.md), [lazy-node-identity-tracking](lazy-node-identity-tracking.md), [per-document-parse-scheduler](per-document-parse-scheduler.md)

## Context

Semantic tokens for a document with code injections (e.g. a Markdown file with
many fenced code blocks) are recomputed *in full* on every `semanticTokens/full`
and `semanticTokens/full/delta` request. The delta request additionally diffs
the freshly computed token vector against the previous one.

A whole-document content-hash cache (`SemanticTokenCache`, shipped in PR #530)
already short-circuits the case where the *entire* document text is unchanged —
so an idle re-request of `semanticTokens/full` is free. **That cache cannot help
the steady-state typing path**: every keystroke changes the document text, so
its content-hash key always misses and the hot path recomputes from scratch. The
`semanticTokens/full/delta`-after-edit request is therefore the path this
decision targets — editing one character inside one injected block re-parses and
re-queries the host plus *every* injection region in the document.

### Where the per-keystroke time actually goes

Measured on the `full/delta`-after-edit path for a single-character,
single-region edit (production-faithful: incremental `tree.edit` +
`reparse(Some(seed))`, then the token recompute, then the delta diff), release
build, warm, on Markdown with N fenced code blocks (rust/lua/yaml):

| phase | 60 blocks | 150 blocks | 300 blocks | cacheable here? |
|---|---|---|---|---|
| incremental host parse | 0.06 ms | 0.17 ms | 0.37 ms | no — already incremental & cheap |
| host token collect | 0.45 | 1.28 | 2.88 | no |
| **injection discovery** | 0.45 | 1.28 | 2.92 | not in v1 (see Companion lever) |
| **per-region tokenize** | **0.75** | **1.66** | **3.27** | **yes — the target** |
| finalize sweep line | 0.18 | 0.50 | 1.10 | no |
| delta diff | 0.002 | 0.004 | 0.008 | no — negligible |
| **per-keystroke total** | **1.89** | **4.90** | **10.54** | |

Two structural facts fall out:

1. **The delta diff and incremental parse are nearly free.** The per-keystroke
   CPU is almost entirely the *token recompute* — exactly what a per-region
   cache avoids. (This is also why the whole-doc cache helps `full` but not
   `delta`: both endpoints run the same recompute; the diff that distinguishes
   them costs nothing.)
2. **Per-region tokenize is ~1/3 of the recompute, not all of it.** Host token
   collection and injection *discovery* are each roughly equal in cost, and
   `finalize` adds ~10%. Even a perfect per-region tokenize cache leaves a hard
   floor of host-collect + discovery + finalize on every keystroke.

The codebase already contains most of the machinery to avoid this, but only its
*invalidation* half is wired up:

- `InjectionMap` (`src/analysis/semantic_cache.rs`): a per-URI `rust_lapper`
  interval tree of `CacheableInjectionRegion`s. Populated on parse
  (`src/lsp/cache.rs` `insert`) and queried via `find_overlapping(uri, start,
  end)` to find which regions an edit touches. Fully wired and tested.
- `CacheableInjectionRegion` carries `region_id` (ULID), `byte_range`,
  `line_range`, `start_column`, and `content_hash` — enough to detect whether a
  region's *content* changed and to re-anchor it after edits elsewhere.
- `InjectionTokenCache`: a per-`(uri, region_id)` token store. On edit,
  `src/lsp/cache.rs` calls `find_overlapping` and `injection_token_cache.remove`
  for each touched region, and `clear_document` on close/reparse — so stale
  entries are evicted correctly.

The gap: `InjectionTokenCache::store` and `::get` are `#[cfg(test)]`-only. In
production nothing ever writes to or reads from the cache, so the eviction logic
guards an always-empty cache. The hot path (`handle_semantic_tokens_full` in
`src/analysis/semantic.rs`) unconditionally calls `collect_host_tokens` +
`collect_injection_tokens_parallel` over the whole document.

This is the largest remaining *typing-path* lever for injection-heavy documents
(the whole-doc cache having already taken the unchanged-document case). The
reason it has not been wired in is a genuine design question, not an oversight:
what to cache, and how to keep coordinates correct when an edit above a region
shifts that region's position in the host document.

## Decision

Wire the injection token cache into the hot path with **region-local `RawToken`s**
as the cached representation, re-anchored to host coordinates at reuse time, and
re-entering the pipeline *before* `finalize_tokens`.

Per request, in `collect_injection_tokens_parallel`:

1. For each injection region (identified by `region_id` via `InjectionMap`),
   check `InjectionTokenCache` for an entry whose `content_hash` matches the
   region's current content.
2. **Hit**: translate the cached region-local `RawToken`s into host coordinates
   using the region's *current* `line_range.start` / `start_column` (which the
   freshly parsed `InjectionMap` already provides), and skip re-parsing /
   re-querying that region.
3. **Miss**: compute the region's tokens as today, then store them back in
   region-local coordinates keyed by `(uri, region_id)` + `content_hash`.
4. Merge cached-and-recomputed injection tokens with freshly computed host
   tokens and run the existing `finalize_tokens` sweep line over the union.

Caching **region-local pre-finalize `RawToken`s** (not post-finalize
`SemanticTokens`) is the load-bearing choice: it keeps the priority / depth /
`node_byte_len` / `pattern_index` metadata the sweep line needs, and makes
re-anchoring a pure `line += region_line_offset` translation rather than a
delta-decode-then-re-encode.

## Considered Options

### A. Cache region-local `RawToken`s, re-enter before finalize (chosen)

Re-anchoring is a single additive line/column offset. The sweep line still sees
the full token set, so host-vs-injection exclusion and transparent-token
breakpoints at region boundaries keep working unchanged. Cost: change
`InjectionTokenCache`'s value type from `SemanticTokens` to `Vec<RawToken>` (or a
region-local newtype), and make `RawToken` storable across requests (it already
holds only owned/`Copy` fields after `perf(semantic): pre-resolve token type to
legend indices` removed the `String`).

### B. Cache post-finalize `SemanticTokens`, splice into output (today's type)

Matches the current `InjectionTokenCache` value type, so no representation
change. Rejected: finalized tokens have lost the sweep-line metadata, so cached
region tokens cannot correctly interact with host tokens or transparent
breakpoints at their boundaries — splicing them in risks wrong winners exactly
at region edges (the subtlety `semantic-token-overlap-resolution` exists to get
right). It also forces a delta-decode + re-encode to re-anchor coordinates.

### C. Whole-document memoization keyed by content hash

Cache the entire finalized result per `(uri, content_hash)`. Trivial to wire,
but only helps when the *whole* document is unchanged — which the existing
`result_id` no-op short-circuit already covers. Provides nothing for the actual
hot case (one region edited, the rest reused). Rejected as redundant.

### D. Leave as-is, optimize allocation/algorithm only

The route taken so far (Arc-shared mappings, binary-search coordinate
conversion, pre-resolved token indices). Real wins, but all O(work) constant-
factor reductions — none removes the *recompute-everything* structure. Kept as
complementary, not a substitute.

## Consequences

### Positive

- Editing one region recomputes one region; the rest are O(token-count) copies
  with an offset. Turns per-edit cost from "reparse whole document" into
  "reparse the edited region", which is the dominant cost for large injected
  documents.
- Reuses infrastructure already built, maintained, and tested (`InjectionMap`,
  `content_hash`, eviction in `src/lsp/cache.rs`).

### Negative

- `RawToken` becomes a cross-request persisted type, so its memory layout and
  any future field changes now affect cache validity, not just one request.
- Re-anchoring logic is a new correctness surface: an off-by-one in the line/
  column offset would mis-place every token in a reused region. Needs targeted
  tests (edit-above-shifts-region, same-line-edit-before-region, region added/
  removed) plus the existing e2e snapshot coverage as a backstop.
- Column re-anchoring is only trivial for the region's *first* line; multi-line
  regions whose `start_column` changes need care (only line-0 columns shift).

### Neutral

- Memory grows by one token vector per live injection region per open document,
  bounded by `clear_document` on close and `remove` on region change.

## Decision–Implementation Gap

Only the invalidation half is implemented today: `InjectionMap` population +
`find_overlapping` + `InjectionTokenCache::{remove, clear_document}` are wired in
`src/lsp/cache.rs`; `InjectionTokenCache::{store, get}` are `#[cfg(test)]`-only
and never called from production. This ADR records the intended design for the
reuse half; until it lands, the cache is maintained-but-unread and the hot path
recomputes in full.
