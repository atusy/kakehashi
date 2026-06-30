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

(Each cell is an independently measured-and-rounded average; the total is the
measured per-keystroke total, so a column may not sum *exactly* to it — e.g. the
300-block phases round-sum to 10.55 vs a measured 10.54.)

Two structural facts fall out:

1. **The delta diff and incremental parse are nearly free.** The per-keystroke
   CPU is almost entirely the *token recompute* — exactly what a per-region
   cache avoids. (The whole-doc cache keys on text alone, so it hits for both
   `full` and `delta` when the document is unchanged and misses for both after
   an edit; the typing path is always a miss, so the recompute — not the
   endpoint — is what matters here.)
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

1. For each injection region (identified by `region_id` from the freshly parsed
   tree's `NodeTracker` — not the cached `InjectionMap`; see the `NodeTracker`
   obligation below), check
   `InjectionTokenCache` for a still-valid entry: its `content_hash` matches the
   region's current content **and** it was stored under the current settings
   generation (see Hazard 2).
2. **Hit**: re-anchor the cached region-local `RawToken`s to the region's
   *current* host position (re-anchor formula below) and skip re-parsing /
   re-querying that region.
3. **Miss**: compute the region's tokens as today, then store them in
   region-local coordinates keyed by `(uri, region_id)`, with `content_hash` and
   the settings generation as the validity check.
4. Merge cached-and-recomputed injection tokens with freshly computed host
   tokens and run the existing `finalize_tokens` sweep line over the union.

Caching **region-local pre-finalize `RawToken`s** (not post-finalize
`SemanticTokens`) is the load-bearing choice: it keeps the priority / depth /
`node_byte_len` / `pattern_index` metadata the sweep line needs, and makes
re-anchoring a pure `line += region_line_offset` translation rather than a
delta-decode-then-re-encode.

### Re-anchoring and the language-agnostic scope (v1)

Injection `RawToken`s today are emitted in **host-absolute** coordinates, with
the per-region host offset baked in at emission (`token_collector.rs`).
Re-anchoring a cached region therefore means reproducing the same host-absolute
tokens after the region has moved. For an arbitrary region this is non-trivial:
row 0 carries the region's `start_column`, deeper rows can carry per-row
host-byte prefixes (e.g. a blockquote `> ` strip), and UTF-16 columns depend on
host line bytes. A same-line edit *before* the region would shift row-0 columns
without changing the region's content hash — a stale-position hazard.

But the re-anchor is **trivial and exact** for any region whose host↔content map
is a pure uniform translation:

- `content_start_col == 0` (the content begins at column 0 of its first line),
  **and**
- the region has no per-row prefix widths (its included ranges are contiguous
  whole lines — no host bytes interleaved per row), **and**
- the region is not part of an `injection.combined` group.

These three conditions are agnostic of the **injected** language — they turn on
the region's coordinate geometry and injection mode, not on which language sits
inside. (They do depend on the *host* grammar's injection query: `combined` and
`include-children` are host-query properties. The point is that no
per-injected-language special-casing is needed.) A fenced code block that starts
at column 0 qualifies; a region that interleaves host bytes per row
(blockquote-style, line-continuation markers, etc.) does not — the predicate
self-selects regardless of which language is injected.

**v1 scope: cache only regions satisfying the predicate; recompute the rest.**
v1 stores **region-local `RawToken`s** (per Decision step 3) keyed by
`(uri, region_id)`, validated by `content_hash` + settings generation. On a hit
it re-anchors to the region's *current* host position:
`host_line = region.line_range.start + token.line`, and
`host_col = token.column` — the row-0 `start_column` term is 0 for every
qualifying region, so no column rebase is needed. This reads only the current region's
already-persisted `line_range.start` / `start_column` from
`CacheableInjectionRegion`: no stored baseline, no delta-decode. The row-0 /
prefix re-anchoring correctness surface is eliminated by construction, and a
same-line-before edit is impossible because the content starts at column 0 on
its own line. Carrying the column rebase to non-qualifying regions is the later,
broader-coverage version; it is not required to land the win.

Two implementation obligations follow, neither inferable from
`CacheableInjectionRegion` alone:

- **Evaluate eligibility on the *current* injection context, at read as well as
  write.** `start_column` is on `CacheableInjectionRegion`, but
  `prefix_byte_widths` and combined-ness live on the transient `InjectionContext`
  that tokenization builds (and `injection.combined` is a query property, not
  range geometry). Re-checking all three clauses against the fresh context means
  a region that has *stopped* qualifying — its start no longer at column 0, or it
  acquired per-row prefixes — is recomputed rather than served a stale hit; only
  regions still satisfying the predicate take the reuse path. (A column-0 fence
  stays eligible even if it gains a column-0 `block_continuation`: column-0
  continuations leave `prefix_byte_widths` empty, so that remains a *valid* hit.)
- **Source `region_id` from the freshly parsed tree's `NodeTracker`, not the
  cached `InjectionMap`.** `InjectionContext` carries no `region_id` today, so v1
  resolves it *inside* `collect_injection_contexts_sync`, where the tree-sitter
  node is in hand — `tracker.get_or_create(uri, start_byte, end_byte, kind)` —
  and carries the ULIDs to the fan-out as a parallel `Vec` aligned with the
  contexts (or as a new `region_id` field on `InjectionContext`). It cannot be
  deferred to the fan-out boundary: `InjectionContext` keeps only
  `host_start_byte`, not the `end_byte` / node `kind` that `get_or_create` needs.
  Either way this threads two new inputs into `collect_injection_tokens_parallel`
  and `handle_semantic_tokens_full`, which take neither today: the document `uri`
  and the `NodeTracker` (owned by the bridge coordinator and passed *into* cache
  methods like `populate_injections`, not held by the `LanguageCoordinator` these
  functions receive). This is safe
  because `apply_input_edits` updates the `NodeTracker` synchronously under the
  edit lock the moment an edit lands — *before* the off-ingress reparse — so the
  ULIDs are consistent even while the cached `InjectionMap` is still pre-edit
  (see [lazy-node-identity-tracking](lazy-node-identity-tracking.md)). Reading
  the `NodeTracker` rather than the cached `InjectionMap` is what closes the
  window between `invalidate_for_edits` and the off-ingress `populate_injections`
  — and means a fallback parse that skips `populate_injections` stays safe and
  must not be "fixed" by calling it there.

### Companion lever: injection discovery

The per-keystroke table shows injection *discovery* (the injection query over
the whole host tree, run every request to enumerate regions) costs as much as
tokenize and is also recomputed every keystroke. It is not cached by v1, but the
same position-stable `region_id` / `InjectionMap` identity that makes token
reuse possible could let an edit reuse the previous region *set* instead of
re-querying. Caching discovery + tokenize together roughly doubles the
per-keystroke saving (measured projection: ~−1/3 for tokenize alone vs ~−60% for
both at 150–300 blocks — the discovery figure is an optimistic ceiling, since
the tracked region set still costs something to update per edit). Treated as a
follow-up, not part of v1: skipping discovery means trusting the tracked set
rather than re-deriving it from the tree, a larger correctness commitment.

## Considered Options

### A. Cache region-local `RawToken`s, re-enter before finalize (chosen)

Re-anchoring is a single additive line/column offset. The sweep line still sees
the full token set, so host-vs-injection exclusion and transparent-token
breakpoints at region boundaries keep working unchanged. Cost: change
`InjectionTokenCache`'s value type from `SemanticTokens` to `Vec<RawToken>` (or a
region-local newtype), and make `RawToken` storable across requests (it is
`Clone` and holds only owned, borrow-free fields — `TokenKind` carries an owned
`Box<str>` for the not-in-legend case, so there are no lifetimes to outlive the
request).

### B. Cache post-finalize `SemanticTokens`, splice into output (today's type)

Matches the current `InjectionTokenCache` value type, so no representation
change. Rejected: finalized tokens have lost the sweep-line metadata, so cached
region tokens cannot correctly interact with host tokens or transparent
breakpoints at their boundaries — splicing them in risks wrong winners exactly
at region edges (the subtlety `semantic-token-overlap-resolution` exists to get
right). It also forces a delta-decode + re-encode to re-anchor coordinates.

### C. Whole-document memoization keyed by content hash (shipped, complementary)

Cache the entire finalized result per `(uri, content_hash)`. **Shipped as PR
#530** (`SemanticTokenCache`, keyed by
`fnv1a_hash(text) ^ generation.wrapping_mul(FNV_PRIME)`). It
covers the *unchanged-document* case — idle `full` re-requests and no-op deltas.
It does nothing for the typing path, because every keystroke changes the
document hash, which is exactly why this ADR exists. The two are complementary
layers, not alternatives: the whole-doc cache is the cheap outer gate, the
per-region cache is the inner reuse on the typing path.

### D. Leave as-is, optimize allocation/algorithm only

The route taken so far (Arc-shared mappings, binary-search coordinate
conversion, pre-resolved token indices). Real wins, but all O(work) constant-
factor reductions — none removes the *recompute-everything* structure. Kept as
complementary, not a substitute.

## Correctness hazards

Today's `get` does no read-time validity check (it keys on `(uri, region_id)`
only), so the cache is **push-based / evict-on-mutation**: every mutation path
that can change a region's tokens must evict its entry before it is re-served.
v1 adds a `content_hash` gate on read (Decision step 1), but `content_hash` is
text-only — it does *not* change when a region's surrounding *context* changes,
so that gate alone misses the first two hazards below. The existing eviction
(edit-overlap, content/language change, region removed, close) covers most
paths; an audit surfaced four the original design did not address:

1. **`injection.combined` sibling cross-contamination (showstopper).** Combined
   injections are discovered as separate `region_id`s but tokenized *jointly*
   (multiple blocks parsed as one document). Editing one block does not change a
   sibling's content hash, so the sibling would serve a hit computed in the
   *old* joint context. **Mitigation (v1): exclude combined groups from the
   cache** — the v1 predicate already does, and the vendored Markdown query
   reserves `combined` for HTML anyway.

   *Caching them later (follow-up)* means treating the whole group as one cache
   unit, which takes three pieces: (a) **one identity** — a composite
   `injection_id` formed from the members' `region_id`s (e.g. joined with `-`),
   so a membership change (a block added/removed) changes the id and misses
   naturally; (b) **a group-wide `content_hash`** over *all* members, so editing
   any one member invalidates the group — this is the part that actually closes
   the cross-contamination, since a composite id alone still hits when the member
   set is unchanged but one member's bytes changed; and (c) **per-member
   re-anchoring**, because the group spans disjoint host ranges and an edit in
   the host gap *between* members shifts later members by a different offset than
   earlier ones — so the trivial single `line += Δ` does not apply. (`region_id`s
   are minted in the cache layer from byte ranges, not on the semantic combined
   context, so each member's id is resolved the same way as a single region —
   see the `NodeTracker` obligation above.)
2. **Config / query reload (showstopper).** `bump_semantic_token_generation`
   (PR #530) clears the whole-doc cache but never touches `InjectionTokenCache`,
   which has no generation in its key. A query/settings reload with unchanged
   text would serve tokens computed under the *old* highlight query.
   **Mitigation: fold the settings generation into the entry's validity, the
   same fix PR #530 applied to the whole-doc cache (where the inner cache takes a
   derived `cache_key`). This is the right design — it matches the existing
   pattern and avoids a global flush — but it is not free: wiring the reuse half
   already promotes `store`/`get` from `#[cfg(test)]` to production, extends their
   signatures to thread the validity key, and changes the value type to
   `Vec<RawToken>` (Option A); folding the generation rides along on those same
   signatures. The alternative — a new global `clear()` on `InjectionTokenCache`
   called from `bump_semantic_token_generation` — is a single new method, but
   flushes every region on any reload.**
3. **Parser-load race (guard).** `process_injection_sync` returns an empty
   `Vec<RawToken>` when a region's parser is not yet loaded — indistinguishable
   from a genuinely empty region, so "never store on the parser-missing branch"
   first requires making that branch *observable*. **Mitigation: have
   `process_injection_sync` return a structured result that separates
   parser-missing from genuinely-empty (e.g. `Option`/`Result` or a
   `parser_loaded` flag), or perform the cache write inside `process_injection_sync`
   where the parser's presence is explicit — so a parser-missing result is never
   stored.**
4. **Row-0 `start_column` re-anchoring (guard, sidestepped by v1 scope).** A
   same-line-before edit shifts row-0 columns without changing the content hash.
   Out of scope for v1 by the `content_start_col == 0` predicate; required only
   if a later version caches non-column-0 regions.

Stale-position bugs here are the same family as the rejected content-epoch
version gate and the delta-encoding position trap — a stale hit is
*wrong-colored text*, worse than a slow recompute. The v1 scope plus the
two showstopper mitigations are the price of correctness, not optional polish.

## Consequences

### Positive

- On the typing path (where the whole-doc cache is structurally useless),
  editing one region recomputes one region; the rest are O(token-count) copies
  with a line offset. Measured ceiling at 150–300 injection blocks: ~−1/3 of
  per-keystroke compute for tokenize-only, up to ~−60% with the discovery
  companion.
- Reuses infrastructure already built, maintained, and tested (`InjectionMap`,
  `content_hash`, eviction in `src/lsp/cache.rs`).

### Negative

- `RawToken` becomes a cross-request persisted type, so its memory layout and
  any future field changes now affect cache validity, not just one request.
- Re-anchoring logic is a new correctness surface; the v1 scope bounds it to a
  trivial `line += Δ`, but still needs targeted tests (edit-above-shifts-region,
  region added/removed) plus the existing e2e snapshot coverage as a backstop.
- **The win is bounded.** Host-collect + discovery + finalize stay on every
  keystroke (~2/3 of the recompute for tokenize-only v1). On fast hardware the
  absolute saving is sub-frame until documents are very large — this lever is
  for very large injected documents and/or slower hardware, not a universal
  speedup.

### Neutral

- Memory grows by one token vector per live injection region per open document,
  bounded by `clear_document` on close and `remove` on region change.

## Decision–Implementation Gap

Only the invalidation half is implemented today: `InjectionMap` population +
`find_overlapping` + `InjectionTokenCache::{remove, clear_document}` are wired in
`src/lsp/cache.rs`; `InjectionTokenCache::{store, get}` are `#[cfg(test)]`-only
and never called from production. The whole-document layer (Option C) shipped
separately as PR #530.

This ADR records the intended design for the per-region reuse half — scoped to
the language-agnostic translation predicate (re-evaluated at read time), gated on
the two showstopper mitigations (exclude `injection.combined`; fold the settings
generation into injection-cache validity) — plus the injection-discovery
companion as a follow-up. Until it lands, the cache is maintained-but-unread and
the typing path recomputes in full.
