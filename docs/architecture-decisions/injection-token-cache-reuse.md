# Injection Token Cache Reuse

<!--
Status: Per-region token reuse IMPLEMENTED (PR #540). The injection-discovery
companion lever is designed but NOT implemented. See "Decision–Implementation Gap".
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

The gap this ADR was written to close (historical — **resolved by PR #540**):
`InjectionTokenCache::store` and `::get` were `#[cfg(test)]`-only, so production
never wrote to or read from the cache and the eviction logic guarded an
always-empty cache; the hot path (`handle_semantic_tokens_full` in
`src/analysis/semantic.rs`) recomputed `collect_host_tokens` +
`collect_injection_tokens_parallel` over the whole document on every request.

This was the largest *typing-path* lever for injection-heavy documents (the
whole-doc cache having already taken the unchanged-document case). It was not
wired in because of a genuine design question, not an oversight: what to cache,
and how to keep coordinates correct when an edit above a region shifts that
region's position in the host document. The rest of this ADR records how that was
resolved (now implemented); the injection-discovery companion lever below is the
remaining, still-unimplemented follow-up.

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
  resolves it *inside* `collect_injection_contexts_sync` — the single-threaded
  discovery phase that runs *before* the Rayon fan-out — where the tree-sitter
  node is in hand: `tracker.get_or_create(uri, start_byte, end_byte, kind)`, with
  the ULIDs carried to the fan-out as an index-aligned `Vec` (or as a new
  `region_id` field on `InjectionContext`). Resolving here, not inside the
  parallel workers, is deliberate: `NodeTracker.entries` is a
  `DashMap<Url, UriEntries>`, so calling `get_or_create` for the same `uri` from
  many workers at once would contend on that document's single write lock — the
  sequential phase sidesteps the contention. It also cannot be deferred to the
  fan-out boundary anyway: `InjectionContext` keeps only `host_start_byte`, not
  the `end_byte` / node `kind` that `get_or_create` needs.
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

### Companion lever: injection discovery (don't-discover-twice)

The per-keystroke table shows injection *discovery* (the injection query over
the whole host tree, run every request to enumerate regions) costs as much as
tokenize and is also recomputed every keystroke. The headline ~−60% projection
above assumed the region *set* could be cached and reused across edits. **That
framing is rejected.** `collect_all_injections` is the structural-change
detector — it is what notices that *this* keystroke added, removed, or split a
fenced block. Reusing a tracked set across an edit means either re-deriving it
from the new tree (no saving) or trusting an incrementally-maintained set without
re-querying (a categorically deeper correctness commitment than the token half,
which never had to answer "what regions exist" — it took the freshly discovered
set as given and only reused per-region *tokens*).

What is sound is a narrower mechanism: **don't run discovery twice on the same
tree.** An edit pays the injection query *twice* — once off-ingress in
`populate_injections` (`src/lsp/cache.rs`) during the reparse, and again,
independently, in the `semanticTokens` request
(`collect_injection_contexts_sync`). Have the semantic request reuse what
`populate_injections` already discovered, **bound to the exact tree it was
discovered on** so it is never served for a different parse.

**Identity, not a content hash.** An earlier draft of this section gated reuse on
`fnv1a(snapshotted_text) + generation`. That is insufficient as the *sole*
correctness backstop, for three converging reasons: a 64-bit hash can collide; it
cannot tell a text `A → B → A` cycle apart from "no change" (so a stale entry from
the first `A`, whose `region_id` the `NodeTracker` may have reissued in between,
could be reused and then *leak* — eviction targets the id `get_or_create` returns
now, not the orphaned stored one); and the cached data is *tree*-derived (points,
ranges, `region_id`s minted against the `NodeTracker`), which text equality only
*implies* via tree-sitter's deterministic incremental parse, it does not
*identify*. So the design ties the discovery to the **tree itself**, two ways in
preference order:

- **Preferred — couple it to the parse result.** Make the discovered contexts
  part of the document's parse result so the semantic path reads `(tree, discovery)`
  from one atomic snapshot — physically one unit, so there is **no reader-side gate
  to get wrong**: a snapshot carries discovery (reuse) or does not (the
  on-demand-parse fallback tree → discover inline). The only gate is on the
  *writer* side — the second CAS below that attaches discovery only to the tree it
  was built for. Realizability, given `populate_injections` runs *after* the
  tree CAS (`set_parse_result_if_text_and_incarnation_unchanged`,
  `src/lsp/lsp_impl/coordinator/parse.rs`):
  - **No *new* `NodeTracker` mutation.** `populate_injections` *already* calls
    `tracker.get_or_create` today, to mint the `region_id` on each
    `CacheableInjectionRegion` it inserts into the `InjectionMap` (which
    `invalidate_for_edits` needs for token-cache eviction). The discovery build
    must **reuse that same id** — it is produced by the same shared stage, from the
    same `Q`, so the lever adds *no* off-ingress `get_or_create` beyond today's.
    (The pre-existing off-ingress minting, and its interaction with a concurrent
    subsequent edit, is governed by
    [lazy-node-identity-tracking](lazy-node-identity-tracking.md) and is neither
    worsened nor in scope to fix here.) Because reuse is bound to the tree
    identity, a stored `region_id` is only ever served for the exact tree it was
    minted on — consistent with the `InjectionMap` / tracker state for that tree;
    a changed tree misses and the miss-path mints fresh, as today.
  - **Publication scheme:** publish the tree first; build discovery; then attach it
    to the parse result via a *second* CAS that no-ops unless the tree / parse
    epoch is still the one just published. Discovery is therefore briefly *absent*
    between the two CASes (a request in that window re-discovers inline), but never
    attached to the wrong tree.
- **Alternative — a parse epoch.** If coupling into the parse result is too
  invasive, key a separate `DiscoveredInjectionCache` on a **fresh monotonic
  per-parse epoch** — *not* `incarnation` (preserved across edits, so it cannot
  distinguish `A→B→A`) and *not* a separately-read "current" epoch (tree
  replacement races the read). The epoch must be stamped onto the tree
  *atomically* in the publication CAS (`(tree, epoch)`), and the semantic path must
  snapshot tree and epoch together; reuse only when the stored discovery's epoch
  equals the snapshot's. An epoch is exact (no collision) and distinct across an
  `A→B→A` cycle.

Either way the settings `generation` still gates the injected-query / highlight
validity (a reload must miss; see correctness surface 7 for the reload-ordering
obligation), and parse ordering (below) affects only hit-rate.

**Currency — hit-rate, not correctness.** Correctness rests on the tree-identity
binding above; ordering only governs how often a reusable discovery is *available*
when the request runs. In the reparse loop, when a tree lands the order is
set-tree → `populate_injections` → `advance_watermark` / `mark_parse_finished`
(`src/lsp/lsp_impl/coordinator/parse.rs`), and the semantic handler's settle waits on the parse
watermark + parse-completion before snapshotting the tree
(`src/lsp/lsp_impl/text_document/semantic_tokens.rs`), so in the common debounced case the
snapshot already carries (or its epoch already matches) the populated discovery.
On the branches where `populate_injections` is skipped (the CAS-fail `if stored`
guard, the no-tree / error paths — `advance_watermark` still runs there), when the
200 ms settle budget expires, or on the on-demand-parse fallback, there is simply
no discovery to reuse and the request re-discovers inline. No ordering guarantee
is load-bearing.

**Mechanism.**

- A single **shared context-build stage** is extracted from
  `collect_injection_contexts_sync`: `collect_all_injections` (the `Q` query) runs
  once and feeds a `build_contexts_from_regions` step that produces, per region,
  `resolved_lang`, `included_ranges` (owned `Vec<Range>`), `prefix_byte_widths`,
  the `combined` flag/grouping, the content byte-range, `host_start_byte`, the
  `region_cache` identity (`region_id` + `validity_hash` + `line_start` +
  `eligible`), the host exclusion-ranges, **and a `discovery_complete` flag**. This
  is the load-bearing refactor: it lets `populate_injections` produce *both* the
  existing `CacheableInjectionRegion` (`R`) *and* the owned discovery — from one
  `Q` and one `get_or_create` per region (the `region_id` is shared between `R` and
  the discovery, so the lever mints no new ids) — rather than calling
  `collect_injection_contexts_sync` (which would run `Q` a second time and defeat
  the whole lever). The semantic miss-path uses the same stage.
- **`R` (the `InjectionMap`) stays complete; only the discovery is gated.** Today
  `from_region_info` builds `R` for *every* region `collect_all_injections`
  returns, regardless of parser/query load state, because `invalidate_for_edits`
  must be able to evict any region. The shared stage must preserve that: it builds
  `R` for all regions, while a region whose parser/highlight query is unavailable
  is dropped only from the *discovery contexts* and taints `discovery_complete`.
  So the refactor must not let the discovery's region-dropping shrink the
  `InjectionMap`.
- `populate_injections` runs that stage, always inserts the complete `R` into the
  `InjectionMap` as today, and stores the owned discovery (coupled to the tree, or
  epoch-tagged) **only when `discovery_complete` is true** (see correctness
  surface 5).
- The `semanticTokens` path, before discovering, takes the discovery carried by
  its tree snapshot (or epoch-matched entry). **Present** → rebuild the
  `InjectionContext`s from the owned data (re-slice `content_text` from the
  current text, look up `highlight_query` fresh by `resolved_lang`, and use the
  stored `region_id`), skipping the `Q` query-match loop *and* the reader's own
  `get_or_create` — the stored id is the one populate already minted for this same
  tree. **Absent** → discover inline via the shared stage as today; never call
  `populate_injections` off the reader path.

The reuse is **whole-document, not eligibility-gated**: `collect_all_injections`
is a single all-or-nothing query over the host tree, so to skip it on a hit
`populate_injections` must store rebuildable contexts for *every* region,
including non-eligible prefixed (blockquote) ones. The `eligible` predicate gates
only the per-region *token* re-anchoring (the shipped half); it does **not**
narrow the discovery reuse. Consequently the `C` that relocates to
`populate_injections` is the full per-region context-build for all regions —
trivial for eligible column-0 fences (no children to exclude), but a real
`compute_included_ranges` node-walk for prefixed regions.

**Why this is the same safety philosophy as the rest of this ADR.** Reuse is only
ever served for discovery bound to the *exact tree* the request is tokenizing
(coupled to the parse result, or epoch-confirmed) under the current settings
`generation` — so a structural change (added / removed / moved fence) produces a
new tree the old discovery is not bound to, and re-discovers. There is no "trust a
stale tracked set" — the deeper commitment the naïve framing required is never
taken on. The tree-identity binding is strictly stronger than the `fnv1a`-hash
`cache_key` the token half (#540) and whole-doc cache (#530) accept; the earlier
content-hash framing for *this* lever was retired in design review precisely
because, as the sole backstop for tree-derived data, a hash identifies text, not
the parse.

**Magnitude (measured).** The saving is *not* a full discovery+context-build —
the per-region context-build (`C`: `compute_included_ranges` node-walks, prefix
widths, combined grouping) is built only in the semantic path today, so the lever
*relocates* `C` into `populate_injections` rather than deduplicating it. What is
genuinely deduplicated is the injection query-match loop (`Q` =
`collect_all_injections`), which runs twice per edit today. A release
microbenchmark (markdown, eligible column-0 lua fences) shows `Q` *dominates* the
discovery bucket, because eligible fences have a trivial `C`:

| blocks | `Q` (query match) | `Q`+`C` (full discovery) | `Q`/(`Q`+`C`) |
|---|---|---|---|
| 150 | 1.18 ms | 1.55 ms | 76% |
| 300 | 2.41 ms | 2.73 ms | 88% |

So the per-edit total-CPU saving ≈ `Q` ≈ 1.2 ms at 150 blocks — comparable to the
shipped per-region-tokenize saving (1.66 ms), and combined with it reaches ≈ −58%
of the per-keystroke budget, confirming this section's original ~−60% projection
in *magnitude* (the projection mis-attributed the cost to the whole bucket, but
`Q` carries it). The benchmark is the *eligible-heavy* case (column-0 fences,
trivial `C`), so the ~0.3 ms relocated `C` at 300 blocks is a best case; a
document dominated by prefixed/blockquote regions relocates a larger `C` to the
off-ingress path (the node-walks), though the per-edit total-CPU saving is still
≈ `Q` regardless of eligibility, since `C` is relocated, not added. Per-*request*
latency improvement is timing-dependent and *not* the figure to quote: it
approaches `Q`+`C` when populate finished during the debounce gap (the request
reuses without waiting) and shrinks toward zero when the request blocks on an
as-yet-unfinished populate (it waits out the work it would otherwise have done
itself). Which regime dominates shifts as parsing moves off-ingress, so the
stable, defensible figure is the ~`Q` total-CPU saving. **No-regression gate:**
it must measure a prefixed-region-heavy document too, not only eligible fences,
since that is where the relocated `C` could lengthen the off-ingress reparse the
semantic settle waits on.

**Open correctness surface (for the design review):** (1) the owned
`included_ranges` are byte/point data computed on the populate tree — valid for
the semantic path *only* under the tree-identity binding (parse-result coupling or
epoch match), which is the whole point of that binding; (2) `highlight_query` /
registry must be looked up fresh at reuse
(don't persist the `Arc<Query>` across a possible reload — the settings
generation already keys the token cache, but the discovery cache must not serve
contexts built under an old query, so the generation has to gate the discovery
cache too); (3) combined groups are stored whole; (4) gated by the same region
count as the token half and measured for no regression, since `populate_injections`
now does strictly more work per reparse even for edits with no following
`semanticTokens` request.

(5) **Transient parser/query-load state must not be cached** — the discovery
analogue of Hazard 3. The shared context-build stage taints
`discovery_complete = false` and *drops* a region when its parser/highlight query
is not yet loaded; this is text-, tree-, and generation-independent, so neither
the tree-identity binding nor the generation catches it. If `populate_injections`
stores discovery built during a load gap, a later request bound to that same tree
is served an **incomplete region set → missing tokens until the next edit**. The
store must therefore be gated on `discovery_complete` (don't persist an incomplete
discovery), exactly as the token half gates `store` on `fully_loaded` — and the
shared stage is what makes that flag available to `populate_injections`, which
does not compute it today. (6) **Lifecycle wiring**: the discovery carried by the
parse result (or the separate `DiscoveredInjectionCache`) must be cleared/replaced
everywhere the existing `injection_map` / `injection_token_cache` are —
`remove_document` on close and the no-query / empty-regions branches of
`populate_injections` (`src/lsp/cache.rs`). Memory grows by a second per-region
owned structure (the `included_ranges` / prefix / exclusion vectors) beyond the
token half's per-region token vector, bounded by the same `clear_document` / close
path (see Consequences/Neutral). (7) **Generation / registry
should be one atomic config epoch (narrow reload race).** The settings `generation`
gate already handles the common reload: a bump means the cached discovery's
generation no longer matches, so it misses and re-discovers. What it does *not*
cover by itself is a `populate_injections` running *concurrently* with the reload:
registry/query mutation currently *precedes* the `bump_semantic_token_generation`
bump (`src/lsp/lsp_impl.rs`), so snapshot-before-build / recheck-after can still
observe new (or mixed) registry state while both reads return the *old* generation,
tagging stale-query discovery as current. The blast radius is small and
self-healing — a transient mis-coloring on the next request after a *concurrent*
reload, gone on the next edit — but closing it cleanly means making registry state
and generation one *indivisible* configuration epoch: an **atomic immutable
registry swap** carrying its generation (a single pointer/`Arc` swap, so no build
ever observes a half-mutated registry), or a
seqlock / config lock that **rejects any snapshot taken mid-mutation**. A bare
"bump before mutation" is *not* sufficient on its own: a build can still snapshot
the already-bumped generation partway through a multi-step registry mutation,
observe mixed state, recheck the same generation, and store it. (This ordering also
tightens the shipped token half, whose
`generation` validity rests on the same assumption; the discovery lever makes it
load-bearing because populate resolves queries and builds contexts off-ingress.)

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
   *old* joint context. **Mitigation (v1):** exclude combined groups from the
   cache — the v1 predicate already does, and the vendored Markdown query
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
   **Mitigation:** fold the settings generation into the entry's validity, the
   same fix PR #530 applied to the whole-doc cache (where the inner cache takes a
   derived `cache_key`). This is preferred for a concrete correctness reason, not
   just neatness: a bare global `clear()` on reload has a store-after-clear race —
   a request already computing when the reload fires captured the pre-bump
   generation and writes its now-stale entry *after* the clear, which the clear
   cannot prevent (exactly why #530 folds a generation into the key instead of
   only clearing; see the `semantic_token_generation` note on `CacheCoordinator`
   in `src/lsp/cache.rs`). The fold is not free — wiring the reuse half already
   promotes `store`/`get` from `#[cfg(test)]` to production, extends their
   signatures to thread the validity key, and changes the value type to
   `Vec<RawToken>` (Option A), and the generation rides along on those same
   signatures. A one-method global `clear()` from `bump_semantic_token_generation`
   is simpler and the flush is cheap on a rare reload, but it trades that
   race-safety away.
3. **Parser-load race (guard).** `process_injection_sync` returns an empty
   `Vec<RawToken>` when a region's parser is not yet loaded — indistinguishable
   from a genuinely empty region, so "never store on the parser-missing branch"
   first requires making that branch *observable*. **Mitigation:** have
   `process_injection_sync` return a structured result that separates
   parser-missing from genuinely-empty (e.g. `Option`/`Result` or a
   `parser_loaded` flag) and let the orchestration layer
   (`collect_injection_tokens_parallel`) make the store decision — so a
   parser-missing result is never stored. Prefer this over writing the cache
   inside `process_injection_sync`: it runs in parallel Rayon workers, so an
   in-function write would thread thread-safe cache-write access into parallel
   tokenization and add side-effects there, whereas a pure structured return
   keeps the store decision at the single-threaded orchestration boundary.
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
- *(Discovery lever, if implemented.)* A second per-region owned structure — the
  `included_ranges` / prefix / exclusion vectors of the stored discovery — beyond
  the token half's per-region token vector, bounded by the same close / reparse
  clear paths.

## Decision–Implementation Gap

The per-region token reuse half (Option A) is **implemented and merged (PR #540)**:
`InjectionTokenCache::{store, get}` are production code keyed by `(uri, region_id)`
with the validity (`validity_hash` = content ⊕ language, plus settings
`generation`) in the value; `collect_injection_tokens_parallel` resolves hits
before the Rayon fan-out and re-anchors them, scoped to the language-agnostic
translation predicate (re-evaluated at read time), region-count-gated, with the
two showstopper mitigations in place (combined groups excluded; settings
generation folded into validity). The whole-document layer (Option C) shipped as
PR #530.

What remains a gap is the **injection-discovery companion lever** (the
don't-discover-twice design above): not implemented. Its shared `Q →
build_contexts_from_regions` stage, the tree-identity binding (parse-result
coupling or a per-parse epoch stamped atomically with the tree), the atomic
config-epoch reload protocol, the `discovery_complete`-gated store, and the
lifecycle wiring are the open work; until it lands, an edit still runs the
injection query twice (off-ingress `populate_injections` + the semantic request).
