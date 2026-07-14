//! Rayon-based parallel injection processing for semantic tokens.
//!
//! This module provides work-stealing parallelism for processing language
//! injections, replacing the previous JoinSet + Semaphore async model.
//!
//! Key design:
//! - Thread-local parser caching (no cross-thread synchronization during parsing)
//! - Work-stealing via Rayon's par_iter() for top-level injections
//! - Sequential processing for nested injections (same thread, no coordination)
//! - One bounded `ComputePool` work unit at the top level

use std::cell::RefCell;
use std::collections::HashMap;

use tree_sitter::{Parser, Tree};
use url::Url;

use super::injection::{InjectionContext, RegionCacheInfo};
use super::token_collector::{ActiveInjectionBounds, RawToken, collect_host_tokens};
use crate::analysis::InjectionTokenCache;
use crate::config::CaptureMappings;
use crate::document::{DiscoveredInjections, DiscoveredRegion, DiscoveredRegionCache};
use crate::language::LanguageCoordinator;
use crate::language::NodeTracker;
use crate::language::injection::{
    CacheableInjectionRegion, InjectionRegionInfo, MAX_INJECTION_DEPTH, compute_included_ranges,
    compute_included_ranges_clipped, effective_offset_for_pattern, has_combined_for_pattern,
    intersect_included_ranges, parse_with_ranges, sub_select_included_ranges,
};
use crate::text::position::byte_to_utf16_col;

/// Minimum number of top-level single injection regions before injection-token
/// caching engages. Below this, `collect_injection_tokens_parallel` runs exactly
/// as it did pre-#529 — no region-id resolution, no content hashing, no store —
/// so small documents (the overwhelming majority) pay zero overhead and are
/// byte-identical to the uncached path. The cache only helps once enough regions
/// exist that reusing the untouched ones outweighs the per-region bookkeeping.
pub(super) const INJECTION_CACHE_MIN_REGIONS: usize = 8;

/// Test-only counter of how many times the discovery-reuse branch fired (skipped
/// the injection query by reusing owned discovery). Lets an in-process server
/// test assert reuse actually engages end-to-end after a real parse+attach —
/// the latency benchmark cannot distinguish "reuse fired" from "reuse never
/// fired, fell back inline" (both read as flat request latency).
#[cfg(test)]
pub(crate) static DISCOVERY_REUSE_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Per-context output of [`process_injection_sync`]: the region's tokens plus
/// whether every parser in its subtree (its own and all nested injections) was
/// loaded. `fully_loaded == false` means at least one region produced an empty
/// token set only because its parser was missing — caching that would serve an
/// incomplete result forever, since the content hash won't change when the
/// parser later loads. The store decision gates on this.
pub(crate) struct InjectionTokens {
    pub tokens: Vec<RawToken>,
    pub fully_loaded: bool,
}

/// Borrowed handle bundling everything the top-level injection pass needs to
/// resolve region identities and read/write the injection-token cache (#529).
/// Threaded as `Option`: `None` disables caching (nested passes, range requests,
/// tests) and reproduces the pre-#529 behavior exactly.
pub(crate) struct InjectionCacheCtx<'a> {
    pub uri: &'a Url,
    pub tracker: &'a NodeTracker,
    pub cache: &'a InjectionTokenCache,
    pub generation: u64,
    pub incarnation: u64,
    /// Whether the snapshot this compute serves was current when the
    /// work-unit started: only then may region ids be MINTED into the
    /// live-coordinate tracker. A stale serve reuses existing ids read-only
    /// and skips caching for regions the tracker does not already know.
    ///
    /// Accepted residual: an edit landing DURING the fan-out can still let
    /// this pass mint from a just-staled snapshot. Unlike the captures walk
    /// (whose ids go out on the wire, so its mints run through the
    /// latch-gated `mint_batch_if_unshifted` reconciliation), region ids
    /// stay internal cache keys — a phantom entry is orphaned, never
    /// resolved, and the token cache stays correct via its content-hash
    /// validity gate — so the latch gating is not worth its cost here.
    pub mint_regions: bool,
    /// Owned discovery for this tree (#529), or `None`. Reused — skipping the
    /// injection query — only when its `generation` still matches `generation`
    /// above (a settings reload bumps the generation, so stale-query discovery
    /// misses and re-discovers inline).
    pub discovery: Option<&'a DiscoveredInjections>,
}

/// Maximum number of parsers to cache per Rayon worker thread.
///
/// This bounds memory usage for long-running LSP servers. The value balances:
/// - Typical workloads (Markdown with 3-5 different injection languages)
/// - Memory per parser (roughly 1-5MB depending on grammar complexity)
/// - Rayon worker threads (typically num_cpus, e.g., 8-16 threads)
///
/// With 8 parsers × 16 threads × 5MB = ~640MB worst case, though typical
/// usage is much lower since most documents use only 1-2 injection languages.
const MAX_CACHED_PARSERS: usize = 8;

/// Simple LRU cache for parsers with bounded size.
///
/// Uses a HashMap for O(1) lookup and a Vec for LRU tracking.
/// When full, evicts the least recently used parser.
struct LruParserCache {
    /// Map from language_id to parser
    parsers: HashMap<String, Parser>,
    /// LRU order: most recently used at the end
    order: Vec<String>,
}

impl LruParserCache {
    fn new() -> Self {
        Self {
            parsers: HashMap::with_capacity(MAX_CACHED_PARSERS),
            order: Vec::with_capacity(MAX_CACHED_PARSERS),
        }
    }

    /// Get a mutable reference to a parser, updating LRU order.
    fn get_mut(&mut self, language_id: &str) -> Option<&mut Parser> {
        if self.parsers.contains_key(language_id) {
            // Move to end of LRU order (most recently used)
            if let Some(pos) = self.order.iter().position(|k| k == language_id) {
                let key = self.order.remove(pos);
                self.order.push(key);
            }
            self.parsers.get_mut(language_id)
        } else {
            None
        }
    }

    /// Insert a parser, evicting LRU entry if at capacity.
    fn insert(&mut self, language_id: String, parser: Parser) {
        if self.parsers.len() >= MAX_CACHED_PARSERS {
            // Evict least recently used (front of order vec)
            if !self.order.is_empty() {
                let lru_key = self.order.remove(0);
                self.parsers.remove(&lru_key);
            }
        }
        self.parsers.insert(language_id.clone(), parser);
        self.order.push(language_id);
    }

    /// Check if language is cached.
    fn contains(&self, language_id: &str) -> bool {
        self.parsers.contains_key(language_id)
    }

    /// Clear the cache.
    fn clear(&mut self) {
        self.parsers.clear();
        self.order.clear();
    }

    /// Get current cache size (for testing).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.parsers.len()
    }
}

// Thread-local parser cache for Rayon worker threads.
//
// Each Rayon worker thread maintains its own bounded LRU cache of parsers.
// This avoids cross-thread synchronization during parallel injection processing
// while preventing unbounded memory growth in long-running LSP servers.
static PARSER_CACHE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

thread_local! {
    static PARSER_CACHE: RefCell<(u64, LruParserCache)> =
        RefCell::new((0, LruParserCache::new()));
}

pub(crate) fn invalidate_thread_local_parser_caches() {
    PARSER_CACHE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Factory for creating parsers with thread-local caching.
///
/// Uses the `LanguageRegistry` to create parsers on demand, caching them
/// in thread-local storage for reuse within the same Rayon worker.
pub(crate) struct ThreadLocalParserFactory {
    registry: crate::language::registry::LanguageRegistry,
}

impl ThreadLocalParserFactory {
    /// Create a new factory with the given language registry.
    pub fn new(registry: crate::language::registry::LanguageRegistry) -> Self {
        Self { registry }
    }

    /// Parse text using a cached parser for the given language, creating and
    /// caching the parser in thread-local storage on first use.
    ///
    /// `included_ranges` (content-text-relative) restricts what the parser sees,
    /// excluding structural markers like blockquote `> ` prefixes. Returns `None`
    /// if the language is not registered or parsing fails.
    pub fn parse(
        &self,
        language_id: &str,
        text: &str,
        included_ranges: Option<&[tree_sitter::Range]>,
    ) -> Option<Tree> {
        PARSER_CACHE.with(|cache| {
            let mut state = cache.borrow_mut();
            let generation = PARSER_CACHE_GENERATION.load(std::sync::atomic::Ordering::Acquire);
            if state.0 != generation {
                state.1.clear();
                state.0 = generation;
            }
            let cache = &mut state.1;

            // Get or create parser for this language
            if !cache.contains(language_id) {
                let language = self.registry.get(language_id)?;
                let mut parser = Parser::new();
                parser.set_language(&language).ok()?;
                cache.insert(language_id.to_string(), parser);
            }

            // Parse using the cached parser, delegating the set/parse/reset
            // protocol to the shared parse_with_ranges function.
            let parser = cache.get_mut(language_id)?;
            parse_with_ranges(
                parser,
                text,
                included_ranges,
                "kakehashi::semantic",
                language_id,
            )
        })
    }

    /// Check if a language is available for parsing.
    #[cfg(test)]
    pub fn has_language(&self, language_id: &str) -> bool {
        self.registry.contains(language_id)
    }

    /// Clear the thread-local parser cache.
    ///
    /// Useful for testing or when languages are reloaded.
    #[cfg(test)]
    pub fn clear_cache() {
        PARSER_CACHE.with(|cache| {
            cache.borrow_mut().1.clear();
        });
    }
}

/// Parse one injection and collect its tokens (plus any nested injections,
/// recursed on the same thread). `depth = 0` is the host document.
/// `host_line_starts` must come from `build_line_start_bytes(host_text)`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn process_injection_sync(
    ctx: &InjectionContext<'_>,
    factory: &ThreadLocalParserFactory,
    coordinator: &LanguageCoordinator,
    capture_mappings: Option<&CaptureMappings>,
    host_text: &str,
    host_lines: &[&str],
    host_line_starts: &[usize],
    depth: usize,
    supports_multiline: bool,
    cancel: Option<&crate::cancel::CancelToken>,
) -> InjectionTokens {
    if crate::cancel::is_cancelled(cancel) {
        return InjectionTokens {
            tokens: Vec::new(),
            fully_loaded: false,
        };
    }

    // Check recursion depth. Hitting the cap is a deliberate truncation, not a
    // missing parser, so the result is still "fully loaded" for caching purposes.
    if depth >= MAX_INJECTION_DEPTH {
        return InjectionTokens {
            tokens: Vec::new(),
            fully_loaded: true,
        };
    }

    // Parse the injection content (with optional included ranges for blockquotes).
    // A missing parser is indistinguishable from an empty region by tokens alone,
    // so flag it: the orchestration layer must not cache a parser-missing result.
    let Some(tree) = factory.parse(
        &ctx.resolved_lang,
        ctx.content_text,
        ctx.included_ranges.as_deref(),
    ) else {
        return InjectionTokens {
            tokens: Vec::new(),
            fully_loaded: false,
        };
    };

    // Discover nested injections BEFORE collecting tokens so we can compute
    // exclusion ranges. This suppresses this level's captures within regions
    // that will be handled by deeper injection languages. Nested regions are
    // never cached in v1, so no cache context is threaded into the recursion.
    let (nested_contexts, nested_exclusion_ranges, nested_discovery_complete) =
        collect_injection_contexts_sync(
            ctx.content_text,
            &tree,
            Some(&ctx.resolved_lang),
            coordinator,
            ctx.host_start_byte,
            ctx.included_ranges.as_deref(),
            None,
            // A large region can contain many nested candidates, so cancellation
            // must remain observable after the outer fan-out entry check.
            cancel,
        );

    let mut tokens = Vec::new();
    // A nested injection dropped during discovery (its parser/query not loaded)
    // leaves this subtree incomplete, so it must not be cached as a hit.
    let mut fully_loaded = nested_discovery_complete;

    // Collect tokens from this injection's highlight query, excluding
    // regions covered by nested injections. Combined contexts force per-line
    // emission: a capture node may span the excluded host text between
    // combined blocks (e.g. a string opened in one block, closed in the
    // next), and only per-line tokens can be clipped back to the blocks.
    if !collect_host_tokens(
        ctx.content_text,
        &tree,
        &ctx.highlight_query,
        Some(&ctx.resolved_lang),
        capture_mappings,
        host_text,
        host_lines,
        host_line_starts,
        ctx.host_start_byte,
        depth,
        supports_multiline && !ctx.combined,
        &nested_exclusion_ranges,
        &ctx.prefix_byte_widths,
        cancel,
        &mut tokens,
    ) {
        return InjectionTokens {
            tokens: Vec::new(),
            fully_loaded: false,
        };
    }
    if ctx.combined {
        clip_tokens_to_included_ranges(&mut tokens, ctx, host_lines, host_line_starts);
    }

    // Recursively process nested injections (same thread, no parallelism). A
    // nested region with a missing parser taints this whole subtree's
    // `fully_loaded`, so the top-level region it belongs to is not cached until
    // every parser it depends on is available.
    for nested_ctx in nested_contexts {
        let nested = process_injection_sync(
            &nested_ctx,
            factory,
            coordinator,
            capture_mappings,
            host_text,
            host_lines,
            host_line_starts,
            depth + 1,
            supports_multiline,
            cancel,
        );
        tokens.extend(nested.tokens);
        fully_loaded &= nested.fully_loaded;
    }

    InjectionTokens {
        tokens,
        fully_loaded,
    }
}

/// Clip a combined context's tokens to its included ranges (#187).
///
/// The combined parse sees the host text between blocks as excluded, but a
/// capture node can still *span* it (a string opened in one block and closed
/// in the next). Its per-line tokens on the excluded rows — markdown prose,
/// fence markers — must not survive, and tokens on boundary lines keep only
/// the columns inside a range. Tokens are per-line here (combined contexts
/// disable multiline emission), so clipping is a per-line interval intersect.
fn clip_tokens_to_included_ranges(
    tokens: &mut Vec<RawToken>,
    ctx: &InjectionContext<'_>,
    host_lines: &[&str],
    host_line_starts: &[usize],
) {
    let Some(ranges) = ctx.included_ranges.as_deref() else {
        return;
    };

    // Allowed UTF-16 column intervals per host line.
    let mut allowed: HashMap<usize, Vec<(usize, usize)>> = HashMap::with_capacity(ranges.len());
    for r in ranges {
        let host_start = ctx.host_start_byte + r.start_byte;
        let host_end = ctx.host_start_byte + r.end_byte;
        if host_start >= host_end {
            continue;
        }
        let first_line = host_line_starts
            .partition_point(|&s| s <= host_start)
            .saturating_sub(1);
        let last_line = host_line_starts
            .partition_point(|&s| s < host_end)
            .saturating_sub(1);
        for line in first_line..=last_line {
            let line_start = host_line_starts.get(line).copied().unwrap_or(0);
            let line_text = host_lines.get(line).copied().unwrap_or("");
            let seg_start = host_start.saturating_sub(line_start).min(line_text.len());
            let seg_end = (host_end - line_start).min(line_text.len());
            // A range ending in the newline still admits the full line; an
            // empty segment (range starting past the line text, or touching
            // the line only at a boundary) admits nothing — skip it instead
            // of recording an interval no token can intersect.
            if seg_start >= seg_end {
                continue;
            }
            let col_start = byte_to_utf16_col(line_text, seg_start);
            let col_end = byte_to_utf16_col(line_text, seg_end);
            allowed.entry(line).or_default().push((col_start, col_end));
        }
    }

    let original = std::mem::take(tokens);
    let mut clipped = Vec::with_capacity(original.len());
    for tok in original {
        let Some(intervals) = allowed.get(&tok.line) else {
            continue;
        };
        let tok_end = tok.column + tok.length;
        for &(a, b) in intervals {
            let s = tok.column.max(a);
            let e = tok_end.min(b);
            if s < e {
                clipped.push(tok.with_span(tok.line, s, e - s));
            }
        }
    }
    *tokens = clipped;
}

/// Collect injection contexts from a parsed tree (sync version).
///
/// This is a synchronous version of the injection context collection that
/// works without mutable parser access. It discovers all injections in the
/// given tree and returns their contexts for processing.
///
/// Returns `(contexts, exclusion_ranges, discovery_complete)`. `exclusion_ranges`
/// are the content-local byte ranges of each resolved injection (regions where
/// child injections produce their own tokens, so parent captures overlapping
/// them should be suppressed). `discovery_complete` is `false` when any injection
/// was dropped because its parser/highlight query was unavailable — a *transient*
/// gap that re-fills when the parser loads without the content changing. A caller
/// that caches a region must taint it with this so an incomplete subtree is never
/// stored (and then served stale once the missing parser arrives, #529 Hazard 3).
#[allow(clippy::too_many_arguments)]
fn collect_injection_contexts_sync<'a>(
    text: &'a str,
    tree: &Tree,
    filetype: Option<&str>,
    coordinator: &LanguageCoordinator,
    content_start_byte: usize,
    parent_included_ranges: Option<&[tree_sitter::Range]>,
    cache_ctx: Option<(&Url, &NodeTracker, u64, bool)>,
    cancel: Option<&crate::cancel::CancelToken>,
) -> (Vec<InjectionContext<'a>>, Vec<(usize, usize)>, bool) {
    use crate::language::injection::collect_all_injections;

    let current_lang = filetype.unwrap_or("unknown");
    let Some(injection_query) = coordinator.injection_query(current_lang) else {
        // No injection query: there is nothing to discover, so discovery is
        // trivially complete (no region was dropped).
        return (Vec::new(), Vec::new(), true);
    };

    let Some(injections) = collect_all_injections(&tree.root_node(), text, Some(&injection_query))
    else {
        return (Vec::new(), Vec::new(), true);
    };

    let mut contexts = Vec::with_capacity(injections.len());
    let mut exclusion_ranges = Vec::with_capacity(injections.len());
    // Tainted to `false` when an injection is skipped because its parser or
    // highlight query isn't loaded yet (vs. a permanently malformed region).
    let mut discovery_complete = true;

    // Partition out `injection.combined` regions: every capture of one
    // (language, pattern) pair parses as a single document so cross-block
    // context survives (#187). Patterns that also carry #offset! stay on the
    // per-region path — offset adjustment and multi-region merging don't
    // compose (same exclusivity the included_ranges handling applies below),
    // and no vendored query combines them.
    let mut singles = Vec::new();
    let mut combined_groups: indexmap::IndexMap<(String, usize), Vec<InjectionRegionInfo>> =
        indexmap::IndexMap::new();
    for injection in injections {
        if has_combined_for_pattern(&injection_query, injection.pattern_index)
            // effective_offset_for_pattern (not the raw directive parser):
            // an all-zero #offset! is a no-op and must not exile the pattern
            // to the singles path — the rest of the codebase branches on the
            // effective offset for the same reason (cf. injection_stack.rs).
            && effective_offset_for_pattern(&injection_query, injection.pattern_index).is_none()
        {
            combined_groups
                .entry((injection.language.clone(), injection.pattern_index))
                .or_default()
                .push(injection);
        } else {
            singles.push(injection);
        }
    }

    // Resolve cache identity only when a cache handle is threaded in (top-level
    // request, not a nested pass) AND the region count clears the gate. The count
    // is `singles.len()` — the raw single captures, an upper bound on the regions
    // that will actually be cacheable (some may later fail bounds/language/query
    // resolution). Below the gate, `region_cache` stays `None` for every context,
    // so the store/reuse path is skipped entirely. The gate only governs whether
    // the (cheap) bookkeeping runs; output is byte-identical to the pre-#529 code
    // either way, so the upper-bound count is a safe, conservative trigger.
    let cache_for_singles = cache_ctx.filter(|_| singles.len() >= INJECTION_CACHE_MIN_REGIONS);

    for injection in singles {
        // Poll for supersession per region: this resolve loop is the dominant
        // discovery cost on a large document (one language resolution + query
        // fetch per injection region), so a newer keystroke must be able to
        // abort it here rather than after all regions are resolved. Return
        // rather than `break`: the `combined_groups` pass below is itself
        // expensive (per-group language resolution + query lookups), so a
        // supersede should skip it too. Discovery is definitionally incomplete
        // here, hence `false`; the caller discards the partial result on cancel.
        if crate::cancel::is_cancelled(cancel) {
            return (contexts, exclusion_ranges, false);
        }

        // Split into a query-independent owned discovery step and a
        // context-rebuild step (the load-bearing refactor for the discovery
        // lever, #529): the same two steps let `populate_injections` produce and
        // store owned discovery, and a later semantic request rebuild contexts
        // from it without re-running the injection query. The steps run
        // back-to-back here, so collect output is unchanged.
        match discover_single_region(
            &injection,
            text,
            coordinator,
            parent_included_ranges,
            cache_for_singles,
            // Inline path: no producer-prebuilt identity to reuse.
            None,
            // Inline path: early-out a query-missing region (no wasted range work).
            true,
        ) {
            SingleDiscovery::Region(region, prebuilt_query) => {
                match rebuild_context(
                    &region,
                    text,
                    content_start_byte,
                    coordinator,
                    &mut exclusion_ranges,
                    prebuilt_query,
                ) {
                    Some(ctx) => contexts.push(ctx),
                    // Highlight query not loaded yet — transient, taint discovery.
                    None => discovery_complete = false,
                }
            }
            // Parser/language not loaded yet — transient, taint discovery.
            SingleDiscovery::Incomplete => discovery_complete = false,
            // Permanently invalid bounds — nothing dropped.
            SingleDiscovery::Skip => {}
        }
    }

    for (_group_key, regions) in combined_groups {
        match build_combined_context(
            &regions,
            text,
            coordinator,
            content_start_byte,
            parent_included_ranges,
            &mut exclusion_ranges,
        ) {
            Ok(Some(ctx)) => contexts.push(ctx),
            // Permanently empty/invalid/clipped-away group — nothing dropped.
            Ok(None) => {}
            // Parser/highlight query for the group isn't loaded yet — a transient
            // drop, same as the single-region case, so taint discovery.
            Err(CombinedDiscoveryIncomplete) => discovery_complete = false,
        }
    }

    (contexts, exclusion_ranges, discovery_complete)
}

/// Outcome of [`discover_single_region`] for one top-level single injection.
enum SingleDiscovery {
    /// The region resolved to a language and owned discovery data. The
    /// `Option<Arc<Query>>` is the already-resolved highlight query on the inline
    /// path (so [`rebuild_context`] reuses it rather than looking it up a second
    /// time), or `None` on the producer path (which stores no query and lets reuse
    /// re-resolve it).
    Region(DiscoveredRegion, Option<std::sync::Arc<tree_sitter::Query>>),
    /// The injection language/parser isn't loaded yet — a *transient* gap.
    /// The inline path taints `discovery_complete`; the producer path DROPS
    /// the region from the stored discovery (sound: the failed load is
    /// negative-cached until a reload, which bumps the generation and
    /// invalidates the store).
    Incomplete,
    /// The region is permanently invalid (out-of-bounds byte range) — nothing is
    /// dropped, so discovery stays complete.
    Skip,
}

/// Build the owned, query-independent discovery for one single injection region:
/// everything `collect_injection_contexts_sync` needs to later reconstruct an
/// `InjectionContext` **except** the highlight query (resolved in
/// [`rebuild_context`]) and the borrowed content slice (stored as a byte range).
///
/// Deliberately does *not* look up the highlight query, so the normal collect
/// path resolves it exactly once (in `rebuild_context`) — no extra lookup versus
/// the pre-refactor code. The reuse path (`populate_injections` → semantic
/// request) runs this once off-ingress and reconstructs contexts from the result,
/// skipping the injection query-match loop entirely.
fn discover_single_region(
    injection: &InjectionRegionInfo<'_>,
    text: &str,
    coordinator: &LanguageCoordinator,
    parent_included_ranges: Option<&[tree_sitter::Range]>,
    cache_for_singles: Option<(&Url, &NodeTracker, u64, bool)>,
    // The producer path's already-built cache identity for THIS region
    // (populate minted the id and hashed the content moments earlier):
    // reused instead of a second tracker op + content hash on the
    // pre-publish critical path, the same pattern as `resolve_from_prebuilt`.
    prebuilt_cacheable: Option<&CacheableInjectionRegion>,
    // On the inline path, skip a region whose highlight query isn't loaded
    // *before* the per-region range/prefix/id work, exactly as the pre-refactor
    // code did (`rebuild_context` would otherwise discard it only after that
    // work). The producer passes `false`: it stores the region regardless and
    // lets reuse re-resolve the query (self-healing), so it must not gate here.
    require_highlight_query: bool,
) -> SingleDiscovery {
    let start = injection.content_node.start_byte();
    let end = injection.content_node.end_byte();

    // Validate bounds
    if start > end || end > text.len() {
        return SingleDiscovery::Skip;
    }

    // Extract injection content for language detection
    let injection_content = &text[start..end];

    // Resolve injection language. A failure here means no parser could be
    // loaded for it yet — a transient gap, so discovery is incomplete.
    let Some((resolved_lang, _)) =
        coordinator.resolve_injection_language(&injection.language, injection_content)
    else {
        return SingleDiscovery::Incomplete;
    };

    // Highlight-query early-out (inline path only): a resolvable language with no
    // loaded highlight query produces no tokens, so skip it before the range/id
    // work, matching the pre-refactor taint-and-continue ordering. Resolve the
    // `Arc<Query>` here (one lookup) and thread it to `rebuild_context`, which then
    // doesn't look it up again. The producer passes `require_highlight_query = false`
    // and stores no query (reuse re-resolves it, self-healing).
    let prebuilt_query = if require_highlight_query {
        match coordinator.highlight_query(&resolved_lang) {
            Some(query) => Some(query),
            None => return SingleDiscovery::Incomplete,
        }
    } else {
        None
    };

    // Offset directive resolved at collection time (single source of truth
    // with the bridge path, which applies it in from_region_info)
    let offset = injection.offset;

    // Calculate effective content range
    let content_node = injection.content_node;
    let (inj_start_byte, inj_end_byte) = if let Some(off) = offset {
        use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        // calculate_effective_range clamps, snaps inward to char
        // boundaries, and normalizes start <= end, so the content slice
        // below cannot panic.
        let effective = calculate_effective_range(text, byte_range, off);
        (effective.start, effective.end)
    } else {
        (content_node.start_byte(), content_node.end_byte())
    };

    // Validate effective range after offset adjustment
    if inj_start_byte > inj_end_byte || inj_end_byte > text.len() {
        return SingleDiscovery::Skip;
    }

    // Compute included ranges for the injection parser (Problem 1: blockquote prefixes).
    // When include_children is false and content_node has named children
    // (e.g., block_continuation), we compute gap ranges so the injection parser
    // only sees actual code content.
    //
    // With an offset directive, content_text starts at inj_start_byte
    // (offset-adjusted) rather than content_node.start_byte(), so gaps are
    // clipped to the effective window and relativized to it (#186). The
    // non-offset path keeps the node-anchored variant, which reuses the
    // node's cached Points instead of rescanning text.
    let from_children = if offset.is_some() {
        compute_included_ranges_clipped(
            &injection.content_node,
            injection.include_children,
            text,
            inj_start_byte..inj_end_byte,
        )
    } else {
        compute_included_ranges(&injection.content_node, injection.include_children)
    };
    let from_parent = parent_included_ranges.and_then(|parent_ranges| {
        sub_select_included_ranges(parent_ranges, inj_start_byte, inj_end_byte)
    });
    let included_ranges = match (from_children, from_parent) {
        (Some(child_ranges), Some(parent_ranges)) => {
            let intersected = intersect_included_ranges(&child_ranges, &parent_ranges);
            if intersected.is_empty() {
                None
            } else {
                Some(intersected)
            }
        }
        (Some(ranges), None) | (None, Some(ranges)) => Some(ranges),
        (None, None) => None,
    };

    // Derive per-line byte prefix widths from included_ranges.
    // Each range's start_point.column tells us how many bytes of prefix
    // (e.g., "> ") precede the actual content on that line.
    let prefix_byte_widths = match &included_ranges {
        Some(ranges) => derive_prefix_byte_widths(ranges),
        None => Vec::new(),
    };

    // Resolve this single region's cache identity (#529). region_id is minted
    // from the RAW content node — byte-identical to populate_injections
    // (cache.rs), which keeps the bridge/tracker identities in sync (token
    // eviction itself is content-addressed and needs no id). Eligibility
    // is this request's translation predicate: column-0 start AND no per-row
    // prefixes (singles are never injection.combined).
    let token_cache = cache_for_singles.and_then(|(uri, tracker, incarnation, mint_regions)| {
        let cacheable_owned;
        let cacheable = match prebuilt_cacheable {
            Some(prebuilt) => prebuilt,
            None => {
                // Mint only when the served snapshot was current (see
                // InjectionCacheCtx::mint_regions): a stale serve's coordinates
                // must not enter the live tracker. Read-only reuse keeps the
                // cache warm for regions the edits did not shift; an unknown
                // region simply goes uncached for this stale serve.
                let region_id = if mint_regions {
                    tracker.get_or_create_for_incarnation(
                        uri,
                        injection.content_node.start_byte(),
                        injection.content_node.end_byte(),
                        injection.content_node.kind(),
                        incarnation,
                    )?
                } else {
                    tracker.lookup_in_layer_for_incarnation(
                        uri,
                        injection.content_node.start_byte(),
                        injection.content_node.end_byte(),
                        injection.content_node.kind(),
                        0,
                        incarnation,
                    )?
                }
                .to_string();
                cacheable_owned =
                    CacheableInjectionRegion::from_region_info(injection, &region_id, text);
                &cacheable_owned
            }
        };
        let eligible = cacheable.start_column == 0 && prefix_byte_widths.is_empty();
        // Content-addressed cache key: content hash folded with the resolved
        // language (one shared definition — populate's eviction sweep must
        // compute the same fold).
        let validity_hash = crate::analysis::semantic_cache::region_validity_hash(
            cacheable.content_hash,
            &resolved_lang,
        );
        Some(DiscoveredRegionCache {
            validity_hash,
            line_start: cacheable.line_range.start,
            eligible,
        })
    });

    SingleDiscovery::Region(
        DiscoveredRegion {
            resolved_lang,
            content_start_byte: inj_start_byte,
            content_end_byte: inj_end_byte,
            included_ranges,
            prefix_byte_widths,
            token_cache,
        },
        prebuilt_query,
    )
}

/// Reconstruct an [`InjectionContext`] from owned [`DiscoveredRegion`] data,
/// pushing the region's parent-token exclusion ranges. Resolves the highlight
/// query fresh (never persisted across a possible settings reload) and re-slices
/// the content from the current `text`; `content_start_byte` is the host byte
/// offset of the text the discovery ran over (0 at the top level).
///
/// Returns `None` when the highlight query isn't loaded yet (a transient gap the
/// caller taints into `discovery_complete`, exactly as the pre-refactor
/// query-missing branch did — and, for a reused discovery, the same query the
/// settings `generation` gate would already have missed on a reload). No
/// exclusion is recorded for a dropped region.
fn rebuild_context<'a>(
    region: &DiscoveredRegion,
    text: &'a str,
    content_start_byte: usize,
    coordinator: &LanguageCoordinator,
    exclusion_ranges: &mut Vec<(usize, usize)>,
    // The highlight query already resolved by `discover_single_region` on the
    // inline path (reused here so the query is looked up once), or `None` on the
    // reuse path, where it is re-resolved fresh (never persisted across a possible
    // settings reload — the generation gate ensures a reload misses).
    prebuilt_query: Option<std::sync::Arc<tree_sitter::Query>>,
) -> Option<InjectionContext<'a>> {
    let inj_start_byte = region.content_start_byte;
    let inj_end_byte = region.content_end_byte;

    // Guard the re-slice: on the collect path the bounds were just validated; on
    // the reuse path they were validated against the tree this discovery is bound
    // to, but a defensive check keeps a corrupt entry from panicking. `get`
    // (below) additionally rejects a mid-codepoint boundary instead of
    // panicking — impossible for tree-sitter-derived offsets on the bound
    // text, so failing closed (drop the region) is the right degradation.
    if inj_start_byte > inj_end_byte {
        return None;
    }
    let content_text = text.get(inj_start_byte..inj_end_byte)?;

    // Highlight query for the resolved language. Missing = transient (loads with
    // the language), so drop the region and let the caller taint discovery.
    let highlight_query = match prebuilt_query {
        Some(query) => query,
        None => coordinator.highlight_query(&region.resolved_lang)?,
    };

    // Record exclusion ranges for parent token suppression (Problem 2: host token
    // leaking). Per-gap when included_ranges are present so
    // compute_active_injection_regions() produces per-line bounds; otherwise the
    // single full content range. Byte ranges are relative to the effective window
    // start (inj_start_byte).
    if let Some(ref ranges) = region.included_ranges {
        // Prose-style content (e.g. markdown_inline) ends flush with its last
        // character, while container content (e.g. code_fence_content) is in
        // practice newline-terminated. Interior prose gaps still carry their
        // line's newline, spanning (L, col)..(L+1, 0) — which
        // filter_by_injection_regions (finalize.rs) reads as a multiline
        // CONTAINER, stripping host gap-fill tokens (e.g. markup.quote) from
        // every paragraph line except the last, so identical lines rendered
        // differently. Trim the newline from prose gaps so each classifies as
        // the single display line it covers. Containers keep their newlines:
        // their host tokens must not leak into code content. Known edge: a
        // container left unterminated at EOF in a file without a trailing
        // newline ends flush and classifies prose until the fence closes.
        // At EOF the two shapes are indistinguishable from the ranges alone,
        // so one of them must misclassify; resolving toward container instead
        // (e.g. by requiring a newline byte after the flush end) would
        // reinstate the per-line highlight asymmetry for a paragraph ending
        // the file without a trailing newline — the worse artifact, since the
        // fence case renders uniformly and self-heals on close.
        // `get` instead of indexing: like the content re-slice above, a
        // corrupt reuse-path entry must degrade (here: to the untrimmed
        // container classification), not panic.
        let ends_with_newline = |end: usize| text.get(..end).is_some_and(|s| s.ends_with('\n'));
        // checked_add everywhere: reuse-path bytes are untrusted, and a
        // corrupt offset must degrade (skip / untrimmed) — release-mode wrap
        // would misclassify, debug-mode overflow would panic.
        let prose = ranges.last().is_some_and(|r| {
            inj_start_byte
                .checked_add(r.end_byte)
                .and_then(|end| text.get(..end))
                .is_some_and(|s| !s.ends_with('\n'))
        });
        for r in ranges {
            let (Some(start), Some(mut end)) = (
                inj_start_byte.checked_add(r.start_byte),
                inj_start_byte.checked_add(r.end_byte),
            ) else {
                continue;
            };
            if prose && ends_with_newline(end) {
                end -= 1;
                if text.get(..end).is_some_and(|s| s.ends_with('\r')) {
                    end -= 1;
                }
            }
            // A newline-only gap (empty prose line) trims to zero width.
            if start < end {
                exclusion_ranges.push((start, end));
            }
        }
    } else {
        exclusion_ranges.push((inj_start_byte, inj_end_byte));
    }

    let region_cache = region.token_cache.as_ref().map(|tc| RegionCacheInfo {
        validity_hash: tc.validity_hash,
        line_start: tc.line_start,
        eligible: tc.eligible,
    });

    Some(InjectionContext {
        resolved_lang: region.resolved_lang.clone(),
        highlight_query,
        content_text,
        host_start_byte: content_start_byte + inj_start_byte,
        included_ranges: region.included_ranges.clone(),
        prefix_byte_widths: region.prefix_byte_widths.clone(),
        combined: false,
        region_cache,
    })
}

/// Build the owned injection discovery for a freshly parsed document from the
/// regions [`collect_all_injections`](crate::language::injection::collect_all_injections)
/// already returned — the producer half of the "don't discover twice" lever
/// (#529). Reuses the caller's single injection-query pass (`Q`, the dominant
/// discovery cost); only the per-region `from_region_info` / `get_or_create`
/// recompute, negligible beside `Q`.
///
/// Returns `None` (store nothing → the semantic path re-discovers inline) when
/// there are fewer single regions than the token-cache gate — caching wouldn't
/// pay, and the reuse path must stay byte-identical to pre-#529. The gate
/// counts `singles` exactly as the inline path's `cache_for_singles` gate does
/// (combined-group regions excluded), so "discovery stored" and "token cache
/// active" are the same predicate — the eviction sweep's live set can never go
/// empty while the inline path is still storing entries.
///
/// A document with an `injection.combined` group (no effective `#offset!`)
/// stores a PARTIAL discovery: the combined group keeps the inline path in v1
/// (its whole-group contexts aren't part of the owned form), so the group is
/// dropped and `complete` is `false` — context reuse falls back inline, while
/// the singles' token-cache identities stay available to the read/store path
/// and the eviction sweep.
///
/// Regions whose language/parser isn't loaded are DROPPED from the stored
/// discovery rather than tainting it: the inline path would produce no tokens
/// for them either, and the failed load is negative-cached until a reload —
/// which bumps the generation, invalidating this discovery — so the store can
/// never mask a later-loadable region.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_document_discovery(
    regions: &[InjectionRegionInfo<'_>],
    // Index-aligned with `regions` (both derive from one
    // `collect_all_injections` pass): the caller's already-minted ids and
    // content hashes, reused instead of a second tracker op + hash per
    // region on the pre-publish critical path.
    prebuilt_cacheable: &[CacheableInjectionRegion],
    injection_query: &tree_sitter::Query,
    text: &str,
    coordinator: &LanguageCoordinator,
    uri: &Url,
    tracker: &NodeTracker,
    generation: u64,
    incarnation: u64,
) -> Option<DiscoveredInjections> {
    // Partition exactly as collect_injection_contexts_sync does: an
    // injection.combined pattern (with no effective #offset!) → drop the
    // region and mark the discovery partial; v1 keeps combined groups on the
    // inline path. The singles are kept — their token-cache identities feed
    // the read/store path and the eviction sweep even when reuse can't fire.
    let mut complete = true;
    let mut singles: Vec<(usize, &InjectionRegionInfo)> = Vec::with_capacity(regions.len());
    for (idx, injection) in regions.iter().enumerate() {
        if has_combined_for_pattern(injection_query, injection.pattern_index)
            && effective_offset_for_pattern(injection_query, injection.pattern_index).is_none()
        {
            log::debug!(
                target: "kakehashi::semantic",
                "discovery stored partial: combined-group region dropped (lang={})",
                injection.language
            );
            complete = false;
            continue;
        }
        singles.push((idx, injection));
    }

    // Same region-count gate as the token half: below it, caching doesn't pay and
    // the semantic reuse path stays byte-identical to the pre-#529 inline path.
    if singles.len() < INJECTION_CACHE_MIN_REGIONS {
        log::debug!(
            target: "kakehashi::semantic",
            "discovery not stored: {} single region(s) below the {} gate",
            singles.len(),
            INJECTION_CACHE_MIN_REGIONS
        );
        return None;
    }

    let mut discovered = Vec::with_capacity(singles.len());
    // Pair by the ORIGINAL region index, not zip: dropped combined regions
    // would otherwise shift every later single onto the wrong prebuilt
    // identity (wrong ULID + content hash).
    for (idx, injection) in singles {
        let prebuilt = &prebuilt_cacheable[idx];
        // Producer: don't gate on the highlight query — store the region and let
        // reuse re-resolve the query (a load without a generation bump self-heals).
        // Producer path: called from populate_injections right after this
        // parse's CAS landed, so the coordinates are current; the region id +
        // content hash are REUSED from the caller's just-built identity
        // (`prebuilt`), not recomputed.
        match discover_single_region(
            injection,
            text,
            coordinator,
            None,
            Some((uri, tracker, incarnation, true)),
            Some(prebuilt),
            false,
        ) {
            // Producer path passes `require_highlight_query = false`, so no query
            // is resolved here (the `None` companion is ignored); reuse resolves it.
            SingleDiscovery::Region(region, _) => discovered.push(region),
            // A language whose parser isn't loadable is DROPPED, matching the
            // inline path's own drop of that region — the reuse output stays
            // byte-identical. Storing (rather than the reference impl's
            // never-store-partial taint) is sound because a failed load can
            // only become a success through a settings/post-install reload
            // (`ensure_language_loaded` negative-caches failures until
            // `load_settings`), and that reload bumps the semantic-token
            // generation — which invalidates this discovery and forces the
            // inline re-discovery that picks the region back up.
            SingleDiscovery::Incomplete => {}
            // Permanently invalid region: dropped from both discovery and the
            // inline contexts, so omitting it keeps reuse equivalent.
            SingleDiscovery::Skip => {}
        }
    }

    Some(DiscoveredInjections {
        generation,
        complete,
        regions: discovered,
    })
}

/// Per-line byte prefix widths from included ranges: each range's
/// `start_point.column` is the byte width of the excluded prefix (e.g. `> `)
/// before the content on that line. Only the first range on a row sets it.
fn derive_prefix_byte_widths(ranges: &[tree_sitter::Range]) -> Vec<usize> {
    // All-zero widths carry no information, and combined groups spanning a
    // large host gap would otherwise allocate a dense O(rows) vector keyed by
    // the last block's relative row. Empty means "no prefixes" downstream.
    if ranges.iter().all(|r| r.start_point.column == 0) {
        return Vec::new();
    }
    let mut widths = Vec::new();
    for r in ranges {
        let row = r.start_point.row;
        if row >= widths.len() {
            widths.resize(row + 1, 0);
        }
        if widths[row] == 0 {
            widths[row] = r.start_point.column;
        }
    }
    widths
}

/// A combined-injection group was dropped because its parser/highlight query
/// isn't loaded yet — a transient gap the caller taints into `discovery_complete`
/// (distinct from a permanent `Ok(None)` empty/invalid group).
struct CombinedDiscoveryIncomplete;

/// Build one [`InjectionContext`] covering every region of an
/// `injection.combined` group (#187): the content slice spans from the first
/// block's start to the last block's end, and `included_ranges` restricts the
/// parser to each block, so cross-block context survives (e.g. an HTML tag
/// opened in one `html_block` and closed in another) while the host text
/// between blocks stays invisible to the injected parser.
///
/// Returns `Ok(None)` for a permanently empty/invalid/clipped-away group (no
/// tokens, nothing dropped) and `Err(CombinedDiscoveryIncomplete)` when the
/// group's language/query isn't loaded yet — a *transient* drop the caller must
/// taint into `discovery_complete` so a parent injection isn't cached with an
/// incomplete combined subtree.
fn build_combined_context<'a>(
    regions: &[InjectionRegionInfo<'_>],
    text: &'a str,
    coordinator: &LanguageCoordinator,
    content_start_byte: usize,
    parent_included_ranges: Option<&[tree_sitter::Range]>,
    exclusion_ranges: &mut Vec<(usize, usize)>,
) -> Result<Option<InjectionContext<'a>>, CombinedDiscoveryIncomplete> {
    // collect_all_injections sorts by start byte, so `first` anchors the group.
    let Some(first) = regions.first() else {
        return Ok(None);
    };
    let group_start = first.content_node.start_byte();
    let group_start_pos = first.content_node.start_position();
    let Some(group_end) = regions.iter().map(|r| r.content_node.end_byte()).max() else {
        return Ok(None);
    };
    if group_start >= group_end || group_end > text.len() {
        return Ok(None);
    }

    // Resolve the language once from the first block's content — the grouping
    // key guarantees every region shares the raw injection language. A missing
    // language/query is transient (loads later without the content changing).
    let first_content = &text[first.content_node.start_byte()..first.content_node.end_byte()];
    let Some((resolved_lang, _)) =
        coordinator.resolve_injection_language(&first.language, first_content)
    else {
        return Err(CombinedDiscoveryIncomplete);
    };
    let Some(highlight_query) = coordinator.highlight_query(&resolved_lang) else {
        return Err(CombinedDiscoveryIncomplete);
    };

    // Rebase an absolute point into the combined content's coordinate space.
    let to_relative_point = |abs: tree_sitter::Point| tree_sitter::Point {
        row: abs.row - group_start_pos.row,
        column: if abs.row == group_start_pos.row {
            abs.column - group_start_pos.column
        } else {
            abs.column
        },
    };

    // Each block contributes its child-exclusion gaps (or its whole node),
    // rebased from node-relative to group-relative coordinates.
    let mut group_ranges: Vec<tree_sitter::Range> = Vec::with_capacity(regions.len());
    for region in regions {
        let node = &region.content_node;
        let node_start = node.start_byte();
        let node_pos = node.start_position();
        let lift_point = |rel: tree_sitter::Point| tree_sitter::Point {
            row: node_pos.row + rel.row,
            column: if rel.row == 0 {
                node_pos.column + rel.column
            } else {
                rel.column
            },
        };
        match compute_included_ranges(node, region.include_children) {
            Some(gaps) => {
                for g in gaps {
                    group_ranges.push(tree_sitter::Range {
                        start_byte: node_start - group_start + g.start_byte,
                        end_byte: node_start - group_start + g.end_byte,
                        start_point: to_relative_point(lift_point(g.start_point)),
                        end_point: to_relative_point(lift_point(g.end_point)),
                    });
                }
            }
            None => group_ranges.push(tree_sitter::Range {
                start_byte: node_start - group_start,
                end_byte: node.end_byte() - group_start,
                start_point: to_relative_point(node_pos),
                end_point: to_relative_point(node.end_position()),
            }),
        }
    }

    // Inherit parent exclusions exactly like the per-region path.
    let from_parent = parent_included_ranges
        .and_then(|pr| sub_select_included_ranges(pr, group_start, group_end));
    let included_ranges = match from_parent {
        Some(parent_ranges) => {
            let intersected = intersect_included_ranges(&group_ranges, &parent_ranges);
            if intersected.is_empty() {
                return Ok(None);
            }
            intersected
        }
        None => group_ranges,
    };

    // Suppress this layer's parent tokens within every combined block.
    // No prose newline-trim here (cf. rebuild_context): every shipped
    // injection.combined capture (html_block etc.) is container content whose
    // ranges are newline-terminated, so the trim would never apply.
    for r in &included_ranges {
        exclusion_ranges.push((group_start + r.start_byte, group_start + r.end_byte));
    }

    let prefix_byte_widths = derive_prefix_byte_widths(&included_ranges);

    Ok(Some(InjectionContext {
        resolved_lang,
        highlight_query,
        content_text: &text[group_start..group_end],
        host_start_byte: content_start_byte + group_start,
        included_ranges: Some(included_ranges),
        prefix_byte_widths,
        combined: true,
        // Combined groups are excluded from the v1 cache (sibling
        // cross-contamination hazard); see the ADR's correctness hazards.
        region_cache: None,
    }))
}

/// Walk top-level injections of the host doc in parallel via Rayon work-stealing,
/// returning `(raw_tokens_sorted_by_position, active_regions)`. Nested injections
/// recurse on the same worker thread — no extra parallelism to avoid coordination
/// overhead.
///
/// `host_line_starts` must come from `build_line_start_bytes(host_text)`; the
/// caller builds it once per request and it is shared by every injection (and
/// the active-region conversion below) so byte→line/col mapping never rescans
/// the host text per injection.
///
/// A region is *active* only if at least one token was produced from it (depth ≥ 1);
/// resolved-but-empty injections don't suppress parent tokens.
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_injection_tokens_parallel(
    host_text: &str,
    host_lines: &[&str],
    host_line_starts: &[usize],
    host_tree: &Tree,
    host_filetype: Option<&str>,
    coordinator: &LanguageCoordinator,
    capture_mappings: Option<&CaptureMappings>,
    supports_multiline: bool,
    cache_ctx: Option<&InjectionCacheCtx>,
    cancel: Option<&crate::cancel::CancelToken>,
) -> (Vec<RawToken>, Vec<ActiveInjectionBounds>) {
    use crate::cancel::is_cancelled;
    use rayon::prelude::*;

    // Reuse the discovery `populate_injections` already ran on this exact tree,
    // when present and still generation-current (#529 companion lever): rebuild
    // the top-level contexts from the owned data and skip the injection query
    // (`Q`) entirely. Otherwise discover inline exactly as before — the fallback
    // is byte-identical to the pre-lever path (below the region-count gate
    // `populate_injections` stores no discovery, so this is `None`; a document
    // with an `injection.combined` group stores a PARTIAL discovery whose
    // `complete: false` fails the filter — either way the inline branch runs,
    // while the partial discovery's singles still feed the token-cache
    // read/store path and the eviction sweep). The reused
    // region ids were minted while their snapshot was current (populate runs
    // behind the landed CAS), so rebuilding from them never writes the tracker —
    // the `mint_regions` stale-serve latch only governs the inline branch.
    let reusable_discovery = cache_ctx.and_then(|c| {
        c.discovery
            .filter(|d| d.generation == c.generation && d.complete)
    });
    let (contexts, exclusion_byte_ranges) = if let Some(discovery) = reusable_discovery {
        #[cfg(test)]
        DISCOVERY_REUSE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut contexts = Vec::with_capacity(discovery.regions.len());
        let mut exclusion_byte_ranges = Vec::with_capacity(discovery.regions.len());
        let completed = visit_until_cancelled(&discovery.regions, cancel, |region| {
            // A region whose highlight query isn't loaded is dropped (rebuild
            // returns None) — same as the inline path's query-missing branch. The
            // generation match makes this near-impossible, but the drop is safe.
            if let Some(ctx) = rebuild_context(
                region,
                host_text,
                0,
                coordinator,
                &mut exclusion_byte_ranges,
                // Reuse path: re-resolve the query fresh (generation-gated).
                None,
            ) {
                contexts.push(ctx);
            }
        });
        if !completed {
            return (Vec::new(), Vec::new());
        }
        (contexts, exclusion_byte_ranges)
    } else {
        // Collect top-level injection contexts and their byte ranges. The cache
        // handle (if any) lets the discovery phase resolve region identities
        // single-threaded, before the fan-out.
        // A top-level region dropped during discovery isn't cached (no region_cache
        // is built for it), so the top-level completeness flag is irrelevant here —
        // only nested completeness, threaded through process_injection_sync, matters.
        let (contexts, exclusion_byte_ranges, _discovery_complete) =
            collect_injection_contexts_sync(
                host_text,
                host_tree,
                host_filetype,
                coordinator,
                0,
                None,
                cache_ctx.map(|c| (c.uri, c.tracker, c.incarnation, c.mint_regions)),
                // `cancel` is polled inside the per-region discovery loop — that
                // resolve loop is the dominant cost on a large document, so a
                // supersede must be able to abort it mid-loop.
                cancel,
            );
        (contexts, exclusion_byte_ranges)
    };

    // Discovery bailed (or genuinely found nothing): either way there is nothing
    // to tokenize. A cancelled discovery returns partial contexts the caller
    // discards, so skip the fan-out entirely.
    if contexts.is_empty() || is_cancelled(cancel) {
        return (Vec::new(), Vec::new());
    }

    // Create factory from coordinator's registry (cloned for thread safety)
    let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_parallel());

    // Threshold for parallel processing - below this, Rayon scheduling overhead exceeds benefit
    const PARALLEL_THRESHOLD: usize = 4;

    // Resolve cache hits before the fan-out (#529, read half): an eligible region
    // whose content hash and settings generation still match a stored entry is
    // re-anchored to its current host line and never re-tokenized; only misses go
    // to the Rayon workers. Eligibility comes from each context's `region_cache`:
    // computed fresh when contexts were discovered inline, or carried in the owned
    // `DiscoveredRegion` when they were rebuilt from reused discovery — identical
    // either way, since snapshot immutability binds the discovery to the same
    // tree (text, tree, and regions are one value) and the generation gate
    // rejects reload-stale discoveries.
    // Below the region-count gate `region_cache` is `None`, so every context misses
    // and the path matches the uncached behavior exactly.
    let mut hit_tokens: Vec<RawToken> = Vec::new();
    let mut miss_indices: Vec<usize> = Vec::with_capacity(contexts.len());
    if let Some(cc) = cache_ctx {
        for (i, ctx) in contexts.iter().enumerate() {
            if let Some(rc) = &ctx.region_cache
                && rc.eligible
                && let Some(local) = cc.cache.get(cc.uri, rc.validity_hash, cc.generation)
            {
                // `reanchor_to_host` mutates in place and each occurrence of
                // this content needs its own re-anchored copy (a duplicate
                // fence sharing this cache entry gets a different
                // `line_start`), so this clone out of the Arc is the one
                // legitimate materialization point — everything upstream
                // (the cache hit itself) stayed O(1).
                hit_tokens.extend(reanchor_to_host((*local).clone(), rc.line_start));
            } else {
                miss_indices.push(i);
            }
        }
    } else {
        miss_indices.extend(0..contexts.len());
    }

    // Tokenize only the misses: parallel above the threshold, sequential below.
    // Each result carries its context index so the store decision can pair it
    // with the region's cache identity off the Rayon workers.
    //
    // A supersede observed at region entry skips the expensive parse+query for
    // that region; the empty stand-in is marked `fully_loaded: false` so the
    // cache store-half below never persists it. Under a rapid-typing pile-up the
    // remaining Rayon closures drain almost immediately once the token trips.
    let process_one = |i: usize| -> (usize, InjectionTokens) {
        if is_cancelled(cancel) {
            return (
                i,
                InjectionTokens {
                    tokens: Vec::new(),
                    fully_loaded: false,
                },
            );
        }
        (
            i,
            process_injection_sync(
                &contexts[i],
                &factory,
                coordinator,
                capture_mappings,
                host_text,
                host_lines,
                host_line_starts,
                1, // depth 1 (first level of injection, host is 0)
                supports_multiline,
                cancel,
            ),
        )
    };
    let processed: Vec<(usize, InjectionTokens)> = if miss_indices.len() >= PARALLEL_THRESHOLD {
        miss_indices.par_iter().map(|&i| process_one(i)).collect()
    } else {
        miss_indices.iter().map(|&i| process_one(i)).collect()
    };

    // Store region-local tokens for newly computed eligible, fully-loaded regions
    // (#529, write half), single-threaded after fan-in so cache writes never run
    // inside a Rayon worker.
    //
    // This store deliberately does NOT re-check `is_cancelled(cancel)`. A region
    // reaches here with `fully_loaded: true` only if its `process_injection_sync`
    // actually completed (the `process_one` entry check already zeroed out
    // regions skipped by a cancel). A completed region's tokens are correct for
    // its content and keyed by `validity_hash` (content + language) and
    // `generation`, so caching them is valid even when this request was
    // superseded mid-pass: a later request hits only when content+generation
    // still match, i.e. only when the tokens are still right. Adding a cancel
    // recheck here would instead drop these valid entries on every keystroke,
    // forcing the next request to recompute them — a regression against the very
    // reuse the cache exists for. (A closed document's residual entry is handled
    // by the cancel-first ordering in `CacheCoordinator::remove_document`.)
    if let Some(cc) = cache_ctx {
        for (i, res) in &processed {
            if let Some(rc) = &contexts[*i].region_cache
                && rc.eligible
                && res.fully_loaded
            {
                let local = to_region_local(&res.tokens, rc.line_start);
                cc.cache
                    .store(cc.uri, rc.validity_hash, cc.generation, local);
            }
        }
    }

    // Merge cache-hit and freshly computed tokens, then sort by position.
    let mut all_tokens: Vec<RawToken> = hit_tokens;
    for (_, res) in processed {
        all_tokens.extend(res.tokens);
    }
    all_tokens.sort_by(|a, b| a.line.cmp(&b.line).then_with(|| a.column.cmp(&b.column)));

    // Convert byte ranges to line/column ActiveInjectionBounds values, but only for
    // regions that actually produced tokens (= "active" injection regions).
    let active_regions = compute_active_injection_regions(
        host_text,
        host_lines,
        host_line_starts,
        &exclusion_byte_ranges,
        &all_tokens,
    );

    (all_tokens, active_regions)
}

/// Visit items until cancellation is observed between items.
///
/// A cancellation that arrives during `visit` is observed before the next
/// item; the current opaque callback is allowed to finish.
fn visit_until_cancelled<T>(
    items: &[T],
    cancel: Option<&crate::cancel::CancelToken>,
    mut visit: impl FnMut(&T),
) -> bool {
    for item in items {
        if crate::cancel::is_cancelled(cancel) {
            return false;
        }
        visit(item);
    }
    true
}

/// Translate an eligible region's host-absolute tokens into region-local
/// coordinates for caching: subtract the region's first host line so a stored
/// entry re-anchors with a single `line += line_start` on reuse. The column is
/// already region-invariant for an eligible region (`start_column == 0`, no
/// per-row prefixes), so only the line shifts. `saturating_sub` guards the
/// (unexpected) case of a token above the region start rather than underflowing.
fn to_region_local(tokens: &[RawToken], line_start: u32) -> Vec<RawToken> {
    let line_start = line_start as usize;
    tokens
        .iter()
        .map(|t| {
            // Invariant: every token of an eligible region sits on or below the
            // region's first host line, so the subtraction never truncates. The
            // saturating_sub is a release-build backstop only.
            debug_assert!(
                t.line >= line_start,
                "injection token at host line {} precedes its region start {}",
                t.line,
                line_start
            );
            t.with_span(t.line.saturating_sub(line_start), t.column, t.length)
        })
        .collect()
}

/// Re-anchor cached region-local tokens to the region's current host position:
/// the inverse of [`to_region_local`]. Because the region is eligible
/// (`start_column == 0`, no per-row prefixes), the column is already correct and
/// only the line shifts by the region's current first host line — so an edit
/// *above* an unchanged region (which leaves its content hash, hence the cache
/// entry, intact but moves it) is re-anchored exactly.
fn reanchor_to_host(mut tokens: Vec<RawToken>, line_start: u32) -> Vec<RawToken> {
    let line_start = line_start as usize;
    // Only the line shifts (column/length/identity unchanged), so mutate in place
    // rather than allocating a fresh vector. `saturating_add` mirrors
    // `to_region_local`'s `saturating_sub` — line numbers never approach usize
    // overflow in practice, but the two stay symmetric and wrap-free.
    for t in &mut tokens {
        t.line = t.line.saturating_add(line_start);
    }
    tokens
}

/// Convert byte-based exclusion ranges to line/column `ActiveInjectionBounds`s,
/// keeping only those regions that contain at least one injection token.
///
/// `tokens` MUST be sorted ascending by `(line, column)` (the caller sorts
/// before invoking). That ordering lets each region binary-search its start
/// position and scan only the tokens inside the region, rather than the whole
/// token list — turning the cost from O(regions × tokens) into
/// O(regions × log n + tokens-in-regions).
fn compute_active_injection_regions(
    host_text: &str,
    host_lines: &[&str],
    line_starts: &[usize],
    byte_ranges: &[(usize, usize)],
    tokens: &[RawToken],
) -> Vec<ActiveInjectionBounds> {
    byte_ranges
        .iter()
        .filter_map(|&(start_byte, end_byte)| {
            // Convert byte range to line/col
            let (start_line, start_col) =
                byte_to_line_col(host_text, host_lines, line_starts, start_byte);
            let (end_line, end_col) =
                byte_to_line_col(host_text, host_lines, line_starts, end_byte);

            // Binary-search the first token at or after the region start, then
            // scan forward only while tokens remain inside the region.
            let start_idx = tokens.partition_point(|t| {
                t.line < start_line || (t.line == start_line && t.column < start_col)
            });
            let has_injection_tokens = tokens[start_idx..]
                .iter()
                .take_while(|t| t.line < end_line || (t.line == end_line && t.column < end_col))
                .any(|t| t.depth >= 1);

            if has_injection_tokens {
                Some(ActiveInjectionBounds {
                    start_line,
                    start_col,
                    end_line,
                    end_col,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Convert a byte offset in host_text to a (line, utf16_col) pair.
///
/// `line_starts` must come from
/// [`build_line_start_bytes`](super::token_collector::build_line_start_bytes)
/// for the same text.
fn byte_to_line_col(
    host_text: &str,
    host_lines: &[&str],
    line_starts: &[usize],
    byte_offset: usize,
) -> (usize, usize) {
    let byte_offset = byte_offset.min(host_text.len());
    // Snap to valid UTF-8 char boundary (tree-sitter always provides valid offsets,
    // but guard defensively against unexpected inputs).
    let byte_offset = {
        let mut b = byte_offset;
        while b > 0 && !host_text.is_char_boundary(b) {
            b -= 1;
        }
        b
    };
    // Largest line whose start byte is <= byte_offset. line_starts[0] == 0, so
    // the predicate holds for at least one element and the subtraction is safe.
    let line = line_starts.partition_point(|&s| s <= byte_offset) - 1;
    let line_start_byte = line_starts[line];
    let col_byte = byte_offset - line_start_byte;
    let line_text = host_lines.get(line).unwrap_or(&"");
    let col_utf16 = byte_to_utf16_col(line_text, col_byte);
    (line, col_utf16)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tree_sitter::Query;

    use super::super::token_collector::build_line_start_bytes;
    use super::*;

    #[test]
    fn reusable_region_visit_stops_on_mid_loop_cancel() {
        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel_after_polls(4);
        let mut visited = Vec::new();

        assert!(!visit_until_cancelled(
            &[0, 1, 2, 3, 4],
            Some(&cancel),
            |item| visited.push(*item)
        ));
        assert_eq!(visited, vec![0, 1, 2]);
    }
    use crate::language::registry::LanguageRegistry;

    fn create_test_registry() -> LanguageRegistry {
        let registry = LanguageRegistry::new();
        registry.register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        registry
    }

    #[test]
    fn test_thread_local_parser_factory_parses_code() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        let code = "fn main() {}";
        let tree = factory.parse("rust", code, None);

        assert!(tree.is_some(), "Should parse registered language");
        let tree = tree.unwrap();
        assert!(
            !tree.root_node().has_error(),
            "Parse tree should not have errors"
        );
    }

    #[test]
    fn reload_generation_clears_thread_local_parsers() {
        ThreadLocalParserFactory::clear_cache();
        let factory = ThreadLocalParserFactory::new(create_test_registry());
        assert!(factory.parse("rust", "fn main() {}", None).is_some());
        PARSER_CACHE.with(|cache| assert_eq!(cache.borrow().1.len(), 1));

        invalidate_thread_local_parser_caches();
        assert!(factory.parse("unknown", "", None).is_none());
        PARSER_CACHE.with(|cache| assert_eq!(cache.borrow().1.len(), 0));
    }

    #[test]
    fn test_thread_local_parser_factory_returns_none_for_unknown() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        let tree = factory.parse("unknown_language", "some code", None);
        assert!(
            tree.is_none(),
            "Should return None for unregistered language"
        );
    }

    #[test]
    fn test_thread_local_parser_factory_caches_parser() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        // Clear cache first to ensure clean state
        ThreadLocalParserFactory::clear_cache();

        // First parse creates and caches parser
        let tree1 = factory.parse("rust", "fn main() {}", None);
        assert!(tree1.is_some());

        // Second parse reuses cached parser
        let tree2 = factory.parse("rust", "fn test() {}", None);
        assert!(tree2.is_some());

        // Both should produce valid parse trees
        assert!(!tree1.unwrap().root_node().has_error());
        assert!(!tree2.unwrap().root_node().has_error());
    }

    #[test]
    fn test_thread_local_parser_factory_clear_cache() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        // Parse to create cached parser
        let _ = factory.parse("rust", "fn main() {}", None);

        // Clear the cache
        ThreadLocalParserFactory::clear_cache();

        // Verify can still parse after cache clear (parser recreated)
        let tree = factory.parse("rust", "fn test() {}", None);
        assert!(tree.is_some(), "Should still parse after cache clear");
    }

    #[test]
    fn test_thread_local_parser_factory_has_language() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        assert!(
            factory.has_language("rust"),
            "Should have registered language"
        );
        assert!(
            !factory.has_language("unknown"),
            "Should not have unregistered language"
        );
    }

    #[test]
    fn test_parser_handles_complex_code() {
        let registry = create_test_registry();
        let factory = ThreadLocalParserFactory::new(registry);

        let code = r#"
            fn main() {
                let x = 42;
                let y = "hello";
                println!("{} {}", x, y);
            }
        "#;

        let tree = factory.parse("rust", code, None);
        assert!(tree.is_some(), "Should parse complex code");
        assert!(
            !tree.unwrap().root_node().has_error(),
            "Complex code should parse without errors"
        );
    }

    // Tests for sync injection processing
    // Note: Full integration tests require LanguageCoordinator setup with search paths,
    // so these are basic structural tests. Full testing is in the parent module.

    #[test]
    fn test_injection_context_struct_fields() {
        // Verify InjectionContext has the expected fields
        let registry = create_test_registry();
        let language = registry.get("rust").unwrap();

        // Create a simple query for testing
        let query = Query::new(&language, "(identifier) @variable").unwrap();

        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query: Arc::new(query),
            content_text: "fn main() {}",
            host_start_byte: 100,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
            combined: false,
            region_cache: None,
        };

        assert_eq!(ctx.resolved_lang, "rust");
        assert_eq!(ctx.content_text, "fn main() {}");
        assert_eq!(ctx.host_start_byte, 100);
    }

    #[test]
    fn test_byte_to_line_col_mid_char_boundary() {
        // Test that byte_to_line_col handles mid-UTF-8-character offsets defensively
        // by snapping to the nearest valid char boundary.
        let text = "あいう"; // Three 3-byte UTF-8 characters (9 bytes total)
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(text);

        // Offset 0 is valid (start of first char)
        let (line, col) = byte_to_line_col(text, &lines, &line_starts, 0);
        assert_eq!(line, 0);
        assert_eq!(col, 0);

        // Offset 1 is mid-character (should snap to 0)
        let (line, col) = byte_to_line_col(text, &lines, &line_starts, 1);
        assert_eq!(line, 0, "Mid-character offset should snap to line 0");
        assert_eq!(col, 0, "Mid-character offset should snap to col 0");

        // Offset 2 is mid-character (should snap to 0)
        let (line, col) = byte_to_line_col(text, &lines, &line_starts, 2);
        assert_eq!(line, 0);
        assert_eq!(col, 0);

        // Offset 3 is valid (start of second char)
        let (line, col) = byte_to_line_col(text, &lines, &line_starts, 3);
        assert_eq!(line, 0);
        assert_eq!(col, 1); // One UTF-16 code unit (Japanese chars are in BMP)

        // Offset 4 is mid-character (should snap to 3)
        let (line, col) = byte_to_line_col(text, &lines, &line_starts, 4);
        assert_eq!(line, 0);
        assert_eq!(col, 1); // Should snap to start of second char
    }

    #[test]
    fn test_byte_to_line_col_multiline() {
        // Exercises the binary-search line lookup across multiple lines, plus a
        // multibyte char so byte offset != utf16 column.
        let text = "ab\ncö\n\nde"; // ö is 2 bytes (U+00F6), 1 utf16 unit
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(text);
        // line_starts: [0, 3, 7, 8] -> starts of "ab", "cö", "", "de"
        assert_eq!(line_starts, vec![0, 3, 7, 8]);

        // 'a' at byte 0 -> line 0 col 0
        assert_eq!(byte_to_line_col(text, &lines, &line_starts, 0), (0, 0));
        // start of line 1 ("cö") at byte 3 -> line 1 col 0
        assert_eq!(byte_to_line_col(text, &lines, &line_starts, 3), (1, 0));
        // byte 5 is the start of the byte after 'ö' begins... 'c'=3, 'ö'=4..6,
        // so byte 6 is end-of-line on line 1 -> col is utf16 width of "cö" = 2
        assert_eq!(byte_to_line_col(text, &lines, &line_starts, 6), (1, 2));
        // empty line 2 at byte 7 -> line 2 col 0
        assert_eq!(byte_to_line_col(text, &lines, &line_starts, 7), (2, 0));
        // 'e' on line 3: 'd'=8, 'e'=9 -> line 3 col 1
        assert_eq!(byte_to_line_col(text, &lines, &line_starts, 9), (3, 1));
    }

    #[test]
    fn test_process_injection_sync_with_simple_code() {
        use crate::config::WorkspaceSettings;

        // Set up coordinator with search paths
        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load rust language
        let load_result = coordinator.ensure_language_loaded("rust");
        if !load_result.success {
            // Skip test if rust parser not available in CI
            eprintln!("Skipping: rust parser not available");
            return;
        }

        let Some(highlight_query) = coordinator.highlight_query("rust") else {
            eprintln!("Skipping: rust highlight query not available");
            return;
        };

        // Create factory with the coordinator's registry
        let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_parallel());

        let code = "fn main() {}";
        let host_text = code;
        let host_lines: Vec<&str> = host_text.lines().collect();

        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query,
            content_text: code,
            host_start_byte: 0,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
            combined: false,
            region_cache: None,
        };

        let tokens = process_injection_sync(
            &ctx,
            &factory,
            &coordinator,
            None,
            host_text,
            &host_lines,
            &build_line_start_bytes(host_text),
            1, // depth 1 (not host document)
            false,
            None,
        )
        .tokens;

        // Should produce some tokens (at minimum "fn" keyword and "main" identifier)
        assert!(
            !tokens.is_empty(),
            "Should produce tokens for Rust code. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_process_injection_sync_respects_max_depth() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let load_result = coordinator.ensure_language_loaded("rust");
        if !load_result.success {
            eprintln!("Skipping: rust parser not available");
            return;
        }

        let Some(highlight_query) = coordinator.highlight_query("rust") else {
            eprintln!("Skipping: rust highlight query not available");
            return;
        };

        let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_parallel());

        let code = "fn main() {}";
        let host_text = code;
        let host_lines: Vec<&str> = host_text.lines().collect();

        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query,
            content_text: code,
            host_start_byte: 0,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
            combined: false,
            region_cache: None,
        };

        // Process at MAX_INJECTION_DEPTH should return empty
        let tokens = process_injection_sync(
            &ctx,
            &factory,
            &coordinator,
            None,
            host_text,
            &host_lines,
            &build_line_start_bytes(host_text),
            MAX_INJECTION_DEPTH,
            false,
            None,
        )
        .tokens;

        assert!(
            tokens.is_empty(),
            "Should return empty at MAX_INJECTION_DEPTH"
        );
    }

    #[test]
    fn process_injection_sync_returns_incomplete_on_mid_region_cancel() {
        let coordinator = LanguageCoordinator::new();
        let registry = create_test_registry();
        let language = registry.get("rust").expect("rust must be registered");
        let highlight_query = Arc::new(
            Query::new(&language, "(identifier) @variable")
                .expect("inline rust highlight query must compile"),
        );
        let factory = ThreadLocalParserFactory::new(registry);
        let declarations = (0..256)
            .map(|index| format!("let value_{index} = {index};"))
            .collect::<Vec<_>>()
            .join("\n");
        let code = format!("fn main() {{\n{declarations}\n}}");
        let host_lines: Vec<&str> = code.lines().collect();
        let ctx = InjectionContext {
            resolved_lang: "rust".to_string(),
            highlight_query,
            content_text: &code,
            host_start_byte: 0,
            included_ranges: None,
            prefix_byte_widths: Vec::new(),
            combined: false,
            region_cache: None,
        };
        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel_after_polls(3);

        let result = process_injection_sync(
            &ctx,
            &factory,
            &coordinator,
            None,
            &code,
            &host_lines,
            &build_line_start_bytes(&code),
            1,
            false,
            Some(&cancel),
        );

        assert!(result.tokens.is_empty());
        assert!(!result.fully_loaded);
        assert!(cancel.is_cancelled());
    }

    /// Returns the search path for tree-sitter grammars.
    fn test_search_path() -> String {
        std::env::var("TREE_SITTER_GRAMMARS").unwrap_or_else(|_| "deps/tree-sitter".to_string())
    }

    /// Collect the exclusion-range text slices `collect_injection_contexts_sync`
    /// pushes for a markdown document, or `None` when a required parser is
    /// unavailable (test skips, matching the file's convention).
    fn markdown_exclusion_slices(text: &str, extra_langs: &[&str]) -> Option<Vec<String>> {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        for lang in ["markdown", "markdown_inline"].iter().chain(extra_langs) {
            if !coordinator.ensure_language_loaded(lang).success {
                eprintln!("Skipping: {lang} parser not available");
                return None;
            }
        }

        let mut parser_pool = coordinator.create_document_parser_pool();
        let mut parser = parser_pool.acquire("markdown")?;
        let tree = parser.parse(text, None)?;
        parser_pool.release("markdown".to_string(), parser);

        let (_contexts, exclusion_ranges, _complete) = collect_injection_contexts_sync(
            text,
            &tree,
            Some("markdown"),
            &coordinator,
            0,
            None,
            None,
            None,
        );

        Some(
            exclusion_ranges
                .iter()
                .map(|&(start, end)| text[start..end].to_string())
                .collect(),
        )
    }

    /// Prose gaps (markdown_inline in a blockquote paragraph) are trimmed of
    /// their trailing newline so every gap classifies as the single display
    /// line it covers — including interior lines, which previously kept the
    /// newline and classified as multiline container spans.
    #[test]
    fn test_exclusion_ranges_blockquote_prose_gaps_trimmed() {
        let Some(slices) = markdown_exclusion_slices("> foo\n> bar baz\n> qux\n", &[]) else {
            return;
        };
        assert_eq!(
            slices,
            ["foo", "bar baz", "qux"],
            "each prose gap should cover exactly its line's content"
        );
    }

    /// CRLF documents trim the full `\r\n` pair, not just the `\n`.
    #[test]
    fn test_exclusion_ranges_blockquote_prose_gaps_trimmed_crlf() {
        let Some(slices) = markdown_exclusion_slices("> foo\r\n> bar\r\n", &[]) else {
            return;
        };
        assert_eq!(
            slices,
            ["foo", "bar"],
            "CRLF prose gaps should lose both the \\r and the \\n"
        );
    }

    /// Container content (a closed code fence in a blockquote) keeps its
    /// newline-terminated gaps: host tokens must not gap-fill code content.
    #[test]
    fn test_exclusion_ranges_blockquote_code_fence_untrimmed() {
        let Some(slices) = markdown_exclusion_slices("> ```lua\n> local x = 1\n> ```\n", &["lua"])
        else {
            return;
        };
        assert!(
            slices
                .iter()
                .any(|s| s.contains("local x = 1") && s.ends_with('\n')),
            "closed-fence code gaps should keep their trailing newline; got {slices:?}"
        );
    }

    /// Documented edge (accepted, see rebuild_context): a fence left
    /// unterminated at EOF in a file without a trailing newline ends flush,
    /// so its gaps classify prose and are trimmed until the fence closes.
    #[test]
    fn test_exclusion_ranges_unterminated_fence_at_eof_classifies_prose() {
        let Some(slices) =
            markdown_exclusion_slices("> ```lua\n> local x = 1\n> local y = 2", &["lua"])
        else {
            return;
        };
        assert!(
            slices.iter().any(|s| s == "local x = 1"),
            "flush-at-EOF fence gaps are trimmed (prose classification); got {slices:?}"
        );
    }

    // Tests for parallel token collection

    #[test]
    fn test_collect_injection_tokens_parallel_empty_document() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load markdown (host language)
        let load_result = coordinator.ensure_language_loaded("markdown");
        if !load_result.success {
            eprintln!("Skipping: markdown parser not available");
            return;
        }

        // Parse an empty markdown document
        let text = "";
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Collect tokens - should be empty for empty document
        let host_lines: Vec<&str> = text.lines().collect();
        let (tokens, _regions) = collect_injection_tokens_parallel(
            text,
            &host_lines,
            &build_line_start_bytes(text),
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        );

        assert!(tokens.is_empty(), "Empty document should have no tokens");
    }

    #[test]
    fn test_collect_injection_tokens_parallel_with_lua_block() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        // Load both markdown and lua
        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        // Markdown with a Lua code block
        let text = r#"# Hello

```lua
local x = 42
```
"#;

        // Parse the markdown document
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        // Collect tokens in parallel
        let host_lines: Vec<&str> = text.lines().collect();
        let (tokens, _regions) = collect_injection_tokens_parallel(
            text,
            &host_lines,
            &build_line_start_bytes(text),
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        );

        // Should have tokens from the Lua injection
        assert!(
            !tokens.is_empty(),
            "Should have tokens from Lua injection. Got: {:?}",
            tokens
        );

        // Look for the "local" keyword token (should be at line 3, col 0)
        let (keyword_type, keyword_mods) =
            crate::analysis::semantic::legend::map_capture_to_token_type_and_modifiers("keyword")
                .unwrap();
        let has_local_keyword = tokens.iter().any(|t| {
            t.line == 3
                && t.column == 0
                && t.kind
                    == crate::analysis::semantic::token_collector::TokenKind::Mapped(
                        keyword_type,
                        keyword_mods,
                    )
        });

        assert!(
            has_local_keyword,
            "Should have 'local' keyword token at line 3, col 0. Got: {:?}",
            tokens
        );
    }

    #[test]
    fn test_collect_injection_tokens_offset_with_child_exclusion() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        // #186: #offset! WITHOUT injection.include-children. The row offset
        // trims the first content line; blockquote `> ` prefixes
        // (block_continuation children) must still be excluded within the
        // remaining window.
        let md_language = coordinator
            .language_registry_for_parallel()
            .get("markdown")
            .expect("markdown language must be loaded");
        let query = Query::new(
            &md_language,
            r#"
            ((fenced_code_block
               (info_string (language) @injection.language)
               (code_fence_content) @injection.content)
              (#offset! @injection.content 1 0 0 0))
            "#,
        )
        .expect("valid offset injection query");
        coordinator
            .query_store()
            .insert_injection_query("markdown".to_string(), Arc::new(query));

        // Line 0: > ```lua
        // Line 1: > local a = 1   (trimmed by the offset)
        // Line 2: > local b = 2   (only surviving content line)
        // Line 3: > ```
        let text = "> ```lua\n> local a = 1\n> local b = 2\n> ```\n";
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let host_lines: Vec<&str> = text.lines().collect();
        let (tokens, regions) = collect_injection_tokens_parallel(
            text,
            &host_lines,
            &build_line_start_bytes(text),
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        );

        // The surviving `local` keyword maps to host line 2, col 2 (after the
        // stripped `> ` prefix).
        let (keyword_type, keyword_mods) =
            crate::analysis::semantic::legend::map_capture_to_token_type_and_modifiers("keyword")
                .unwrap();
        assert!(
            tokens.iter().any(|t| {
                t.line == 2
                    && t.column == 2
                    && t.kind
                        == crate::analysis::semantic::token_collector::TokenKind::Mapped(
                            keyword_type,
                            keyword_mods,
                        )
            }),
            "Should have 'local' keyword token at line 2, col 2. Got: {:?}",
            tokens
        );

        // No injection token may land on the excluded `> ` prefix.
        assert!(
            tokens.iter().all(|t| !(t.line == 2 && t.column < 2)),
            "No token may cover the excluded blockquote prefix. Got: {:?}",
            tokens
        );

        // Active regions must start after the prefix, proving the exclusion
        // ranges were clipped to the post-offset window rather than dropped.
        assert!(
            regions
                .iter()
                .any(|r| r.start_line == 2 && r.start_col == 2),
            "Active region should start at line 2 col 2 (post-offset, post-prefix). Got: {:?}",
            regions
        );
    }

    #[test]
    fn test_collect_injection_tokens_parallel_tokens_sorted() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return;
        }

        // Multiple Lua code blocks at different positions
        let text = r#"# Doc

```lua
local a = 1
```

More text

```lua
local b = 2
```
"#;

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            eprintln!("Skipping: failed to parse document");
            return;
        };
        parser_pool.release("markdown".to_string(), parser);

        let host_lines: Vec<&str> = text.lines().collect();
        let (tokens, _regions) = collect_injection_tokens_parallel(
            text,
            &host_lines,
            &build_line_start_bytes(text),
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        );

        // Verify tokens are sorted by position
        let mut prev_line = 0usize;
        let mut prev_col = 0usize;
        for token in &tokens {
            assert!(
                token.line > prev_line || (token.line == prev_line && token.column >= prev_col),
                "Tokens should be sorted by (line, column). Got line {} col {} after line {} col {}",
                token.line,
                token.column,
                prev_line,
                prev_col
            );
            prev_line = token.line;
            prev_col = token.column;
        }
    }

    // Tests for LRU cache behavior

    #[test]
    fn test_lru_parser_cache_basic_operations() {
        let mut cache = LruParserCache::new();

        // Initially empty
        assert!(!cache.contains("rust"));
        assert_eq!(cache.len(), 0);

        // Create a test parser
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();

        // Insert
        cache.insert("rust".to_string(), parser);
        assert!(cache.contains("rust"));
        assert_eq!(cache.len(), 1);

        // Get mutable reference
        let parser_ref = cache.get_mut("rust");
        assert!(parser_ref.is_some());

        // Unknown language returns None
        assert!(cache.get_mut("unknown").is_none());
    }

    #[test]
    fn test_lru_parser_cache_eviction() {
        let mut cache = LruParserCache::new();

        // Fill cache to MAX_CACHED_PARSERS
        for i in 0..MAX_CACHED_PARSERS {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            cache.insert(format!("lang{}", i), parser);
        }

        assert_eq!(cache.len(), MAX_CACHED_PARSERS);
        assert!(cache.contains("lang0")); // First inserted (LRU)

        // Insert one more - should evict lang0 (least recently used)
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        cache.insert("lang_new".to_string(), parser);

        assert_eq!(cache.len(), MAX_CACHED_PARSERS);
        assert!(!cache.contains("lang0"), "LRU entry should be evicted");
        assert!(cache.contains("lang_new"), "New entry should be present");
        assert!(cache.contains("lang1"), "Second-oldest should still exist");
    }

    #[test]
    fn test_lru_parser_cache_access_updates_order() {
        let mut cache = LruParserCache::new();

        // Insert two parsers
        for lang in ["lang0", "lang1"] {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            cache.insert(lang.to_string(), parser);
        }

        // Access lang0 to make it recently used
        let _ = cache.get_mut("lang0");

        // Fill the rest of the cache
        for i in 2..MAX_CACHED_PARSERS {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_rust::LANGUAGE.into())
                .unwrap();
            cache.insert(format!("lang{}", i), parser);
        }

        // Insert one more - should evict lang1 (now LRU), not lang0
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        cache.insert("lang_new".to_string(), parser);

        assert!(
            cache.contains("lang0"),
            "Recently accessed lang0 should not be evicted"
        );
        assert!(!cache.contains("lang1"), "LRU lang1 should be evicted");
    }

    #[test]
    fn test_lru_parser_cache_clear() {
        let mut cache = LruParserCache::new();

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        cache.insert("rust".to_string(), parser);

        assert_eq!(cache.len(), 1);

        cache.clear();

        assert_eq!(cache.len(), 0);
        assert!(!cache.contains("rust"));
    }

    /// Shared setup for the injection.combined tests: a coordinator with
    /// markdown + lua loaded and markdown's injection query replaced by one
    /// that marks lua fences `injection.combined` (the vendored query reserves
    /// combined for html blocks, whose parser isn't in the test language set).
    /// Returns `None` (→ skip) when parsers are unavailable.
    fn combined_lua_coordinator() -> Option<LanguageCoordinator> {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);

        let md_result = coordinator.ensure_language_loaded("markdown");
        let lua_result = coordinator.ensure_language_loaded("lua");
        if !md_result.success || !lua_result.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return None;
        }

        let md_language = coordinator
            .language_registry_for_parallel()
            .get("markdown")
            .expect("markdown language must be loaded");
        let query = Query::new(
            &md_language,
            r#"
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content
              (#set! injection.combined)
              (#set! injection.include-children))
            "#,
        )
        .expect("valid combined injection query");
        coordinator
            .query_store()
            .insert_injection_query("markdown".to_string(), Arc::new(query));
        Some(coordinator)
    }

    /// Two lua fences separated by markdown prose.
    ///
    /// Byte map:
    ///   "```lua\n"        0..7
    ///   "local x = 1\n"   7..19   (block 1 content)
    ///   "```\n"          19..23
    ///   "\n"             23..24
    ///   "plain text\n"   24..35
    ///   "\n"             35..36
    ///   "```lua\n"       36..43
    ///   "local y = 2\n"  43..55   (block 2 content)
    ///   "```\n"          55..59
    const COMBINED_LUA_DOC: &str =
        "```lua\nlocal x = 1\n```\n\nplain text\n\n```lua\nlocal y = 2\n```\n";

    #[test]
    fn test_combined_injections_group_into_single_context() {
        let Some(coordinator) = combined_lua_coordinator() else {
            return;
        };

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let tree = parser
            .parse(COMBINED_LUA_DOC, None)
            .expect("markdown must parse");
        parser_pool.release("markdown".to_string(), parser);

        let (contexts, exclusions, _complete) = collect_injection_contexts_sync(
            COMBINED_LUA_DOC,
            &tree,
            Some("markdown"),
            &coordinator,
            0,
            None,
            None,
            None,
        );

        assert_eq!(
            contexts.len(),
            1,
            "combined regions of one (language, pattern) must merge into a single context"
        );
        let ctx = &contexts[0];
        assert_eq!(ctx.resolved_lang, "lua");
        assert_eq!(
            ctx.host_start_byte, 7,
            "combined content must start at the first block's content"
        );
        assert_eq!(ctx.content_text, &COMBINED_LUA_DOC[7..55]);

        let ranges = ctx
            .included_ranges
            .as_ref()
            .expect("combined context must carry per-block included ranges");
        let byte_ranges: Vec<(usize, usize)> =
            ranges.iter().map(|r| (r.start_byte, r.end_byte)).collect();
        assert_eq!(
            byte_ranges,
            vec![(0, 12), (36, 48)],
            "ranges must cover each block's content, relative to the combined start"
        );

        // Host tokens over both blocks must be suppressed.
        assert!(
            exclusions.contains(&(7, 19)) && exclusions.contains(&(43, 55)),
            "each combined block must register an exclusion range, got {:?}",
            exclusions
        );
    }

    /// The **vendored** markdown injection query marks `html_block` combined —
    /// the real-world case #187 was filed for (PHP/ERB-style cross-block
    /// elements). Unlike the synthetic lua setup above, this exercises the
    /// shipped query end to end: an element opened in one `html_block` and
    /// closed in another must merge into a single html context so the html
    /// parser sees one document.
    #[test]
    fn test_vendored_html_block_injection_combines_across_blocks() {
        use crate::config::WorkspaceSettings;

        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _summary = coordinator.load_settings(&settings);
        let md_result = coordinator.ensure_language_loaded("markdown");
        let html_result = coordinator.ensure_language_loaded("html");
        if !md_result.success || !html_result.success {
            eprintln!("Skipping: markdown or html parser not available");
            return;
        }

        // `<div>` opens in the first html block and closes in the second;
        // markdown prose separates them.
        let doc = "<div>\n\nsome *prose*\n\n</div>\n";
        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let tree = parser.parse(doc, None).expect("markdown must parse");
        parser_pool.release("markdown".to_string(), parser);

        let (contexts, _exclusions, _complete) = collect_injection_contexts_sync(
            doc,
            &tree,
            Some("markdown"),
            &coordinator,
            0,
            None,
            None,
            None,
        );

        // The vendored query injects more than html (markdown_inline for the
        // prose); only the html contexts matter here.
        let html_contexts: Vec<&InjectionContext> = contexts
            .iter()
            .filter(|c| c.resolved_lang == "html")
            .collect();
        assert_eq!(
            html_contexts.len(),
            1,
            "vendored html_block combined pattern must merge into one context, got {:?}",
            contexts
                .iter()
                .map(|c| (&c.resolved_lang, c.host_start_byte))
                .collect::<Vec<_>>()
        );
        let ctx = html_contexts[0];
        assert!(
            ctx.content_text.contains("<div>") && ctx.content_text.contains("</div>"),
            "combined html content must span both blocks, got {:?}",
            ctx.content_text
        );
        let ranges = ctx
            .included_ranges
            .as_ref()
            .expect("combined context must carry per-block included ranges");
        assert_eq!(
            ranges.len(),
            2,
            "one included range per html block, got {:?}",
            ranges
        );
    }

    #[test]
    fn test_combined_injection_tokens_keep_host_coordinates() {
        let Some(coordinator) = combined_lua_coordinator() else {
            return;
        };

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let tree = parser
            .parse(COMBINED_LUA_DOC, None)
            .expect("markdown must parse");
        parser_pool.release("markdown".to_string(), parser);

        let host_lines: Vec<&str> = COMBINED_LUA_DOC.lines().collect();
        let (tokens, _regions) = collect_injection_tokens_parallel(
            COMBINED_LUA_DOC,
            &host_lines,
            &build_line_start_bytes(COMBINED_LUA_DOC),
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        );

        let (keyword_type, keyword_mods) =
            crate::analysis::semantic::legend::map_capture_to_token_type_and_modifiers("keyword")
                .unwrap();
        let local_keyword_lines: Vec<usize> = tokens
            .iter()
            .filter(|t| {
                t.column == 0
                    && t.kind
                        == crate::analysis::semantic::token_collector::TokenKind::Mapped(
                            keyword_type,
                            keyword_mods,
                        )
            })
            .map(|t| t.line)
            .collect();
        assert!(
            local_keyword_lines.contains(&1) && local_keyword_lines.contains(&7),
            "combined parse must emit 'local' keywords at host lines 1 and 7, got lines {:?} from {:?}",
            local_keyword_lines,
            tokens
        );
    }

    /// A lua long string opened in block 1 (`[[`) and closed in block 2 (`]]`).
    /// Only a combined parse makes block 2 valid lua, and the spanning string
    /// node covers the markdown prose between the blocks.
    ///
    /// Host lines:
    ///   0 "```lua"   1 "local s = [["   2 "```"   3 ""   4 "prose here"
    ///   5 ""   6 "```lua"   7 "]]"   8 "print(s)"   9 "```"
    const COMBINED_SPANNING_DOC: &str =
        "```lua\nlocal s = [[\n```\n\nprose here\n\n```lua\n]]\nprint(s)\n```\n";

    #[test]
    fn test_combined_injection_does_not_leak_tokens_into_gap_lines() {
        let Some(coordinator) = combined_lua_coordinator() else {
            return;
        };

        let mut parser_pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = parser_pool.acquire("markdown") else {
            eprintln!("Skipping: markdown parser not available");
            return;
        };
        let tree = parser
            .parse(COMBINED_SPANNING_DOC, None)
            .expect("markdown must parse");
        parser_pool.release("markdown".to_string(), parser);

        let host_lines: Vec<&str> = COMBINED_SPANNING_DOC.lines().collect();
        let (tokens, _regions) = collect_injection_tokens_parallel(
            COMBINED_SPANNING_DOC,
            &host_lines,
            &build_line_start_bytes(COMBINED_SPANNING_DOC),
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            // Multiline client support must not let the spanning string token
            // bleed across the gap either.
            true,
            None,
            None,
        );

        // The combined parse produces lua tokens inside both blocks…
        assert!(
            tokens.iter().any(|t| t.line == 1),
            "expected lua tokens on host line 1, got {:?}",
            tokens
        );
        assert!(
            tokens.iter().any(|t| t.line == 8),
            "expected lua tokens on host line 8 (print call), got {:?}",
            tokens
        );

        // …but none on the markdown lines between the blocks: the spanning
        // string node covers them, and clipping to the included ranges must
        // drop those rows.
        let leaked: Vec<_> = tokens
            .iter()
            .filter(|t| (2..=6).contains(&t.line))
            .collect();
        assert!(
            leaked.is_empty(),
            "combined-layer tokens must not leak into gap lines 2..=6, got {:?}",
            leaked
        );
    }

    /// Markdown with eight lua fences — enough to clear
    /// `INJECTION_CACHE_MIN_REGIONS`. Each block starts at column 0 with no
    /// per-row prefix, so every region is cache-eligible.
    fn eight_lua_block_doc() -> String {
        let mut text = String::new();
        for i in 0..8 {
            text.push_str(&format!("```lua\nlocal x{i} = {i}\n```\n\n"));
        }
        text
    }

    /// Coordinator with markdown + lua loaded, or `None` to skip when the
    /// parsers aren't available in the environment.
    fn markdown_lua_coordinator() -> Option<LanguageCoordinator> {
        use crate::config::WorkspaceSettings;
        let coordinator = LanguageCoordinator::new();
        let settings = WorkspaceSettings {
            search_paths: vec![test_search_path()],
            ..Default::default()
        };
        let _ = coordinator.load_settings(&settings);
        let md = coordinator.ensure_language_loaded("markdown");
        let lua = coordinator.ensure_language_loaded("lua");
        if !md.success || !lua.success {
            eprintln!("Skipping: markdown or lua parser not available");
            return None;
        }
        Some(coordinator)
    }

    fn parse_markdown(coordinator: &LanguageCoordinator, text: &str) -> Tree {
        let mut pool = coordinator.create_document_parser_pool();
        let mut parser = pool.acquire("markdown").expect("markdown parser");
        let tree = parser.parse(text, None).expect("parse markdown");
        pool.release("markdown".to_string(), parser);
        tree
    }

    /// `to_region_local` and `reanchor_to_host` are exact inverses, and a region
    /// that moved (re-anchored at a different `line_start`) shifts uniformly by
    /// the line delta with columns untouched.
    #[test]
    fn region_local_roundtrip_and_shift() {
        let host = vec![
            raw_token_at(5, 2, 3),
            raw_token_at(6, 0, 4),
            raw_token_at(7, 8, 1),
        ];

        // Store at line_start 5, re-anchor at the same start → identical.
        let local = to_region_local(&host, 5);
        assert_eq!(local.iter().map(|t| t.line).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(reanchor_to_host(local.clone(), 5), host);

        // Region moved down 3 lines (edit above): re-anchor shifts every token by
        // +3, columns unchanged.
        let moved = reanchor_to_host(local, 8);
        assert_eq!(
            moved.iter().map(|t| (t.line, t.column)).collect::<Vec<_>>(),
            [(8, 2), (9, 0), (10, 8)]
        );
    }

    /// Build the producer's index-aligned cache identities for `regions`,
    /// the way `populate_injections` does before calling
    /// `build_document_discovery`.
    fn cacheable_for(
        regions: &[crate::language::injection::InjectionRegionInfo<'_>],
        uri: &Url,
        tracker: &NodeTracker,
        text: &str,
    ) -> Vec<CacheableInjectionRegion> {
        regions
            .iter()
            .map(|info| {
                let id = tracker
                    .get_or_create(
                        uri,
                        info.content_node.start_byte(),
                        info.content_node.end_byte(),
                        info.content_node.kind(),
                    )
                    .to_string();
                CacheableInjectionRegion::from_region_info(info, &id, text)
            })
            .collect()
    }

    fn raw_token_at(line: usize, column: usize, length: usize) -> RawToken {
        RawToken {
            line,
            column,
            length,
            kind: crate::analysis::semantic::token_collector::TokenKind::Mapped(1, 0),
            depth: 1,
            pattern_index: 0,
            priority: 100,
            node_byte_len: length,
        }
    }

    /// A stale serve (`mint_regions: false`) must not write region ids into
    /// the live-coordinate tracker: unknown regions go uncached (no phantom
    /// entries), while tokens still match the uncached output. Once ids exist
    /// (a current serve minted them), a stale serve reuses them read-only and
    /// still gets identical tokens.
    #[test]
    fn stale_serve_never_mints_region_ids_and_reuses_live_ones() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let text = eight_lua_block_doc();
        let tree = parse_markdown(&coordinator, &text);
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(&text);

        let uri = Url::parse("file:///stale_region_mint.md").unwrap();
        let cache = InjectionTokenCache::new();
        let tracker = NodeTracker::new();
        let stale = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: false,
            discovery: None,
        };

        let tokens = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&stale),
            None,
        )
        .0;
        assert!(!tokens.is_empty(), "stale serves still tokenize");
        assert_eq!(
            cache.test_keys(&uri).len(),
            0,
            "a stale serve with no known regions must not cache (no ids minted)"
        );

        // A current serve mints; a following stale serve reuses those ids.
        let current = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: None,
        };
        let _ = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&current),
            None,
        );
        assert_eq!(
            cache.test_keys(&uri).len(),
            8,
            "the current serve mints and caches all regions"
        );

        let stale_after = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&stale),
            None,
        )
        .0;
        assert_eq!(
            stale_after, tokens,
            "read-only reuse must not change the tokens"
        );
    }

    /// The cached path must produce byte-identical tokens to the uncached path,
    /// on both the first (all-miss, store) and second (all-hit) request.
    #[test]
    fn injection_cache_reuse_matches_uncached_output() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let text = eight_lua_block_doc();
        let tree = parse_markdown(&coordinator, &text);
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(&text);

        let uncached = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        )
        .0;

        let uri = Url::parse("file:///cache_reuse.md").unwrap();
        let cache = InjectionTokenCache::new();
        let tracker = NodeTracker::new();
        let cc = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: None,
        };

        let first = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;
        assert_eq!(first, uncached, "first (store) pass must match uncached");

        // All eight eligible regions should have been stored.
        assert_eq!(
            cache.test_keys(&uri).len(),
            8,
            "every eligible region should be cached after the first pass"
        );

        let second = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;
        assert_eq!(second, uncached, "second (hit) pass must match uncached");
    }

    /// The discovery-reuse path (skip the injection query, rebuild contexts from
    /// the owned discovery `populate_injections` stored) produces byte-identical
    /// tokens to inline discovery — and a stale generation falls back to inline.
    #[test]
    fn discovery_reuse_matches_inline_output() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let text = eight_lua_block_doc();
        let tree = parse_markdown(&coordinator, &text);
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(&text);
        let uri = Url::parse("file:///discovery_reuse.md").unwrap();
        let cache = InjectionTokenCache::new();
        let tracker = NodeTracker::new();

        // Baseline: inline discovery (no owned discovery threaded in). Reset the
        // reuse counter FIRST and assert the inline call leaves it at 0, so an
        // "always-increment" bug (firing the reuse counter even without reuse)
        // can't slip past the `>= 1` assertion on the reuse call below.
        DISCOVERY_REUSE_HITS.store(0, std::sync::atomic::Ordering::Relaxed);
        let inline = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        )
        .0;
        assert_eq!(
            DISCOVERY_REUSE_HITS.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "inline discovery (no discovery threaded in) must not touch the reuse counter"
        );

        // Build the owned discovery exactly as populate_injections does, on the
        // same tree.
        let injection_query = coordinator
            .injection_query("markdown")
            .expect("markdown injection query");
        let regions = crate::language::injection::collect_all_injections(
            &tree.root_node(),
            &text,
            Some(injection_query.as_ref()),
        )
        .expect("regions");
        let discovery = build_document_discovery(
            &regions,
            &cacheable_for(&regions, &uri, &tracker, &text),
            injection_query.as_ref(),
            &text,
            &coordinator,
            &uri,
            &tracker,
            0,
            0,
        )
        .expect("eight single lua blocks clear the gate and discovery completes");
        assert_eq!(discovery.regions.len(), 8);

        // Reuse path: discovery present and generation-current → skips the query.
        let cc = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: Some(&discovery),
        };
        // Counter still 0 here (top reset; inline asserted 0; build_document_discovery
        // doesn't tokenize), so this reuse call is the only thing that can bump it.
        let reused = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;
        assert_eq!(
            reused, inline,
            "discovery reuse must match inline discovery byte-for-byte"
        );
        // Prove the reuse branch actually fired (skipped Q) rather than silently
        // falling back inline — the safety net the latency bench can't provide.
        assert_eq!(
            DISCOVERY_REUSE_HITS.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "exactly one reuse-branch entry: the single generation-current reuse call"
        );

        // Second reuse pass composes the two #529 halves: the first pass stored
        // eligible region tokens, so now the discovery-reuse path (skip Q) also
        // serves those regions from the token cache (hit), not recompute. Still
        // byte-identical.
        let reused_with_token_hits = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;
        assert_eq!(
            reused_with_token_hits, inline,
            "discovery reuse composed with token-cache hits must still match inline"
        );

        // A stale generation must ignore the discovery and re-discover inline.
        let cc_stale = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 1,
            incarnation: 0,
            mint_regions: true,
            discovery: Some(&discovery),
        };
        let fell_back = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc_stale),
            None,
        )
        .0;
        assert_eq!(
            fell_back, inline,
            "a stale-generation discovery must fall back to inline discovery"
        );
    }

    /// A document that mixes an `injection.combined` group with gate-clearing
    /// single regions stores a PARTIAL discovery: `complete: false` (context
    /// reuse must fall back inline — a singles-only rebuild would drop the
    /// combined captures), but the singles keep their token-cache identities so
    /// the eviction sweep's live set matches what the inline path stores
    /// (before this, any combined group made the sweep wipe every live entry
    /// each populate while the inline path kept re-storing them).
    #[test]
    fn build_document_discovery_partial_on_combined_groups() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let md_language = coordinator
            .language_registry_for_parallel()
            .get("markdown")
            .expect("markdown language must be loaded");
        // Structurally disjoint patterns (no dedupe ambiguity): indented code
        // blocks form a combined lua group; fenced blocks stay singles.
        let query = Query::new(
            &md_language,
            r#"
            ((indented_code_block) @injection.content
              (#set! injection.language "lua")
              (#set! injection.combined))

            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
            "#,
        )
        .expect("valid mixed injection query");
        let injection_query = Arc::new(query);
        coordinator
            .query_store()
            .insert_injection_query("markdown".to_string(), Arc::clone(&injection_query));

        // Eight single lua fences (clears the gate) with the two combined
        // indented blocks INTERLEAVED — one before the singles, one mid-list —
        // so a zip-style pairing of the filtered singles against the
        // index-aligned prebuilt identities would shift every later single
        // onto the wrong identity (the off-by-one this test discriminates).
        let mut text = String::from("prose\n\n    local y1 = 1\n\n");
        for i in 0..4 {
            text.push_str(&format!("```lua\nlocal x{i} = {i}\n```\n\n"));
        }
        text.push_str("prose\n\n    local y2 = 2\n\n");
        for i in 4..8 {
            text.push_str(&format!("```lua\nlocal x{i} = {i}\n```\n\n"));
        }

        let tree = parse_markdown(&coordinator, &text);
        let regions = crate::language::injection::collect_all_injections(
            &tree.root_node(),
            &text,
            Some(injection_query.as_ref()),
        )
        .expect("regions");
        assert_eq!(regions.len(), 10, "8 fences + 2 indented blocks");

        let uri = Url::parse("file:///combined_partial.md").unwrap();
        let tracker = NodeTracker::new();
        let prebuilt = cacheable_for(&regions, &uri, &tracker, &text);
        let discovery = build_document_discovery(
            &regions,
            &prebuilt,
            injection_query.as_ref(),
            &text,
            &coordinator,
            &uri,
            &tracker,
            0,
            0,
        )
        .expect("gate-clearing singles must store a (partial) discovery");
        assert!(
            !discovery.complete,
            "a dropped combined group must mark the discovery partial"
        );
        assert_eq!(
            discovery.regions.len(),
            8,
            "singles kept, combined group dropped"
        );
        // Identity alignment: each stored single must carry the validity hash
        // of ITS OWN prebuilt identity (by original region index). A zip over
        // the filtered singles would pair post-combined singles with the
        // dropped combined regions' hashes and fail this set equality.
        let expected_hashes: std::collections::HashSet<u64> = regions
            .iter()
            .zip(prebuilt.iter())
            .filter(|(r, _)| !has_combined_for_pattern(injection_query.as_ref(), r.pattern_index))
            .map(|(_, c)| {
                crate::analysis::semantic_cache::region_validity_hash(c.content_hash, "lua")
            })
            .collect();
        let stored_hashes: std::collections::HashSet<u64> = discovery
            .regions
            .iter()
            .map(|r| {
                r.token_cache
                    .as_ref()
                    .expect("singles keep their token-cache identities for the eviction sweep")
                    .validity_hash
            })
            .collect();
        assert_eq!(
            stored_hashes, expected_hashes,
            "stored singles must keep their own (index-aligned) cache identities"
        );

        // The partial discovery must NOT be consumed by the context-reuse
        // path: if it were, the combined group's tokens would be missing.
        // Byte-equality against the inline pass is the discriminating check
        // (the indented blocks are valid lua, so they contribute tokens).
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(&text);
        let inline = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        )
        .0;
        let indented_lines: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains("local y"))
            .map(|(i, _)| i)
            .collect();
        assert!(
            inline.iter().any(|t| indented_lines.contains(&t.line)),
            "the combined indented blocks must contribute tokens for the check to discriminate"
        );
        let cache = InjectionTokenCache::new();
        let cc = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: Some(&discovery),
        };
        let with_partial = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;
        assert_eq!(
            with_partial, inline,
            "a partial discovery must fall back to inline discovery (combined tokens intact)"
        );
    }

    /// A region whose injected language can't be resolved to a parser is
    /// DROPPED from the stored discovery — matching the inline path, which
    /// drops it too, so reuse output stays byte-identical. Storing (rather
    /// than the former never-store-partial taint) is sound because a failed
    /// load is negative-cached until a settings/post-install reload, and that
    /// reload bumps the semantic-token generation, invalidating this
    /// discovery. Here EVERY region is unresolvable, so the stored discovery
    /// is present but empty.
    #[test]
    fn build_document_discovery_stores_empty_when_every_region_unresolvable() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let injection_query = coordinator
            .injection_query("markdown")
            .expect("markdown injection query");
        // Fences of a language with no parser anywhere: `collect_all_injections`
        // still returns them (the query captures whatever the info string says),
        // but `resolve_injection_language` fails → SingleDiscovery::Incomplete.
        let mut text = String::from("# Unresolvable\n\n");
        for i in 0..10 {
            text.push_str(&format!("```zzznotalang\ncontent {i}\n```\n\n"));
        }
        let mut pool = coordinator.create_document_parser_pool();
        let Some(mut parser) = pool.acquire("markdown") else {
            return;
        };
        let tree = parser.parse(text.as_str(), None).expect("parse markdown");
        pool.release("markdown".to_string(), parser);
        let regions = crate::language::injection::collect_all_injections(
            &tree.root_node(),
            &text,
            Some(injection_query.as_ref()),
        )
        .expect("regions");
        let uri = Url::parse("file:///incomplete_bail.md").unwrap();
        let tracker = NodeTracker::new();
        let discovery = build_document_discovery(
            &regions,
            &cacheable_for(&regions, &uri, &tracker, &text),
            injection_query.as_ref(),
            &text,
            &coordinator,
            &uri,
            &tracker,
            0,
            0,
        )
        .expect("discovery is stored with the unresolvable regions dropped");
        assert!(
            discovery
                .regions
                .iter()
                .all(|r| r.resolved_lang == "markdown_inline"),
            "the unresolvable fenced regions are dropped; only the document's \
             own markdown_inline regions remain"
        );
        assert!(
            discovery.regions.len() < regions.len(),
            "the ten zzznotalang fences must not be stored"
        );
    }

    /// The dominant per-compute cost on a large document is the single-threaded
    /// discovery loop (one language resolution + query fetch per region), so a
    /// superseded request must abort there rather than pay the full discovery.
    /// Assert a pre-cancelled token collapses discovery to zero contexts while an
    /// uncancelled run finds all eight. (A pre-set flag can't distinguish a
    /// per-iteration check from one hoisted just above the loop — both yield
    /// zero — but it does catch the checkpoint being removed entirely, which
    /// would let all eight regions resolve under cancellation.)
    #[test]
    fn cancelled_token_aborts_injection_discovery() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let text = eight_lua_block_doc();
        let tree = parse_markdown(&coordinator, &text);

        let (live, _excl, _complete) = collect_injection_contexts_sync(
            &text,
            &tree,
            Some("markdown"),
            &coordinator,
            0,
            None,
            None,
            None,
        );
        assert_eq!(
            live.len(),
            8,
            "sanity: the eight lua blocks are discovered without cancellation"
        );

        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel();
        let (aborted, _excl, _complete) = collect_injection_contexts_sync(
            &text,
            &tree,
            Some("markdown"),
            &coordinator,
            0,
            None,
            None,
            Some(&cancel),
        );
        assert!(
            aborted.is_empty(),
            "a pre-cancelled token must break the discovery loop before resolving any region"
        );
    }

    /// End-to-end: a cancelled injection pass must produce no tokens AND persist
    /// nothing to the region cache. A pre-cancelled token bails at the
    /// post-discovery guard (before the fan-out), so this pins the outer-guard
    /// path. The fan-out's own cancel skip is narrower: `process_one`'s entry
    /// check zeroes only regions whose tokenization had NOT started when the
    /// token flipped (returning `fully_loaded: false`, which the #529 store gate
    /// then drops). A region already inside `process_injection_sync` continues
    /// polling through nested discovery and highlight collection; if it observes
    /// cancellation it returns `fully_loaded: false` and is not stored. Contrast
    /// the uncancelled run in `injection_cache_reuse_matches_uncached_output`,
    /// which stores all eight regions.
    #[test]
    fn cancelled_token_stores_no_injection_regions() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let text = eight_lua_block_doc();
        let tree = parse_markdown(&coordinator, &text);
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(&text);

        let uri = Url::parse("file:///cancel_no_store.md").unwrap();
        let cache = InjectionTokenCache::new();
        let tracker = NodeTracker::new();
        let cc = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: None,
        };

        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel();
        let (tokens, regions) = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            Some(&cancel),
        );
        assert!(tokens.is_empty(), "a cancelled pass must yield no tokens");
        assert!(regions.is_empty(), "a cancelled pass must yield no regions");
        assert_eq!(
            cache.test_keys(&uri).len(),
            0,
            "a cancelled pass must not persist any (partial) region to the cache"
        );
    }

    /// Proof the reuse path actually *reads* the cache rather than recomputing:
    /// overwrite one region's stored entry with a sentinel token (length 999,
    /// which no real lua token has), then re-run. The sentinel can only appear in
    /// the output if the region was served from the cache.
    #[test]
    fn injection_cache_reuse_serves_stored_tokens() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let text = eight_lua_block_doc();
        let tree = parse_markdown(&coordinator, &text);
        let lines: Vec<&str> = text.lines().collect();
        let line_starts = build_line_start_bytes(&text);

        let uri = Url::parse("file:///cache_sentinel.md").unwrap();
        let cache = InjectionTokenCache::new();
        let tracker = NodeTracker::new();
        let cc = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: None,
        };

        // Populate.
        let _ = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        );

        // Overwrite one region with a region-local sentinel token.
        let (ch, generation) = cache
            .test_keys(&uri)
            .into_iter()
            .next()
            .expect("a region should be cached");
        let sentinel = RawToken {
            line: 0,
            column: 0,
            length: 999,
            kind: crate::analysis::semantic::token_collector::TokenKind::Mapped(1, 0),
            depth: 1,
            pattern_index: 0,
            priority: 100,
            node_byte_len: 999,
        };
        cache.store(&uri, ch, generation, vec![sentinel]);

        let tokens = collect_injection_tokens_parallel(
            &text,
            &lines,
            &line_starts,
            &tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;

        assert!(
            tokens.iter().any(|t| t.length == 999),
            "the sentinel token must surface, proving the reuse path read the cache"
        );
    }

    /// An edit *above* an unchanged region must re-anchor its cached tokens to the
    /// new host line. With the tracker positions adjusted (as production does in
    /// did_change), the cached entry stays valid and is shifted by the inserted
    /// line — matching a fresh uncached compute of the edited document.
    #[test]
    fn injection_cache_reanchors_after_edit_above() {
        let Some(coordinator) = markdown_lua_coordinator() else {
            return;
        };
        let old_text = eight_lua_block_doc();
        let old_tree = parse_markdown(&coordinator, &old_text);
        let old_lines: Vec<&str> = old_text.lines().collect();
        let old_line_starts = build_line_start_bytes(&old_text);

        let uri = Url::parse("file:///cache_reanchor.md").unwrap();
        let cache = InjectionTokenCache::new();
        let tracker = NodeTracker::new();
        let cc = InjectionCacheCtx {
            uri: &uri,
            tracker: &tracker,
            cache: &cache,
            generation: 0,
            incarnation: 0,
            mint_regions: true,
            discovery: None,
        };

        // Populate against the original document.
        let _ = collect_injection_tokens_parallel(
            &old_text,
            &old_lines,
            &old_line_starts,
            &old_tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        );

        // Insert a blank line at the very top: every region shifts down one line,
        // none of their content changes. Adjust the tracker positions the way
        // did_change does, so the stored entries stay addressable (stable ULIDs).
        let new_text = format!("\n{old_text}");
        tracker.apply_text_diff(&uri, &old_text, &new_text);
        let new_tree = parse_markdown(&coordinator, &new_text);
        let new_lines: Vec<&str> = new_text.lines().collect();
        let new_line_starts = build_line_start_bytes(&new_text);

        let cached = collect_injection_tokens_parallel(
            &new_text,
            &new_lines,
            &new_line_starts,
            &new_tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            Some(&cc),
            None,
        )
        .0;

        let uncached = collect_injection_tokens_parallel(
            &new_text,
            &new_lines,
            &new_line_starts,
            &new_tree,
            Some("markdown"),
            &coordinator,
            None,
            false,
            None,
            None,
        )
        .0;

        assert_eq!(
            cached, uncached,
            "re-anchored cached tokens must match a fresh compute of the edited doc"
        );
    }
}
