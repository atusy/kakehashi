//! Rayon-based parallel injection processing for semantic tokens.
//!
//! This module provides work-stealing parallelism for processing language
//! injections, replacing the previous JoinSet + Semaphore async model.
//!
//! Key design:
//! - Thread-local parser caching (no cross-thread synchronization during parsing)
//! - Work-stealing via Rayon's par_iter() for top-level injections
//! - Sequential processing for nested injections (same thread, no coordination)
//! - Single spawn_blocking bridge at the top level

use std::cell::RefCell;
use std::collections::HashMap;

use tree_sitter::{Parser, Tree};

use super::injection::InjectionContext;
use super::token_collector::{ActiveInjectionBounds, RawToken, collect_host_tokens};
use crate::config::CaptureMappings;
use crate::language::LanguageCoordinator;
use crate::language::injection::{
    InjectionRegionInfo, MAX_INJECTION_DEPTH, compute_included_ranges, has_combined_for_pattern,
    intersect_included_ranges, parse_offset_directive_for_pattern, parse_with_ranges,
    sub_select_included_ranges,
};
use crate::text::position::byte_to_utf16_col;

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
    #[cfg(test)]
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
thread_local! {
    static PARSER_CACHE: RefCell<LruParserCache> = RefCell::new(LruParserCache::new());
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
            let mut cache = cache.borrow_mut();

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
            cache.borrow_mut().clear();
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
) -> Vec<RawToken> {
    // Check recursion depth
    if depth >= MAX_INJECTION_DEPTH {
        return Vec::new();
    }

    // Parse the injection content (with optional included ranges for blockquotes)
    let Some(tree) = factory.parse(
        &ctx.resolved_lang,
        ctx.content_text,
        ctx.included_ranges.as_deref(),
    ) else {
        return Vec::new();
    };

    // Discover nested injections BEFORE collecting tokens so we can compute
    // exclusion ranges. This suppresses this level's captures within regions
    // that will be handled by deeper injection languages.
    let (nested_contexts, nested_exclusion_ranges) = collect_injection_contexts_sync(
        ctx.content_text,
        &tree,
        Some(&ctx.resolved_lang),
        coordinator,
        ctx.host_start_byte,
        ctx.included_ranges.as_deref(),
    );

    let mut tokens = Vec::new();

    // Collect tokens from this injection's highlight query, excluding
    // regions covered by nested injections. Combined contexts force per-line
    // emission: a capture node may span the excluded host text between
    // combined blocks (e.g. a string opened in one block, closed in the
    // next), and only per-line tokens can be clipped back to the blocks.
    collect_host_tokens(
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
        &mut tokens,
    );
    if ctx.combined {
        clip_tokens_to_included_ranges(&mut tokens, ctx, host_lines, host_line_starts);
    }

    // Recursively process nested injections (same thread, no parallelism)
    for nested_ctx in nested_contexts {
        let nested_tokens = process_injection_sync(
            &nested_ctx,
            factory,
            coordinator,
            capture_mappings,
            host_text,
            host_lines,
            host_line_starts,
            depth + 1,
            supports_multiline,
        );
        tokens.extend(nested_tokens);
    }

    tokens
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
/// Returns `(contexts, exclusion_ranges)` where exclusion_ranges are the
/// content-local byte ranges of each resolved injection. These ranges
/// correspond to the regions where child injections produce their own tokens,
/// so parent captures overlapping these ranges should be suppressed.
fn collect_injection_contexts_sync<'a>(
    text: &'a str,
    tree: &Tree,
    filetype: Option<&str>,
    coordinator: &LanguageCoordinator,
    content_start_byte: usize,
    parent_included_ranges: Option<&[tree_sitter::Range]>,
) -> (Vec<InjectionContext<'a>>, Vec<(usize, usize)>) {
    use crate::language::injection::collect_all_injections;

    let current_lang = filetype.unwrap_or("unknown");
    let Some(injection_query) = coordinator.injection_query(current_lang) else {
        return (Vec::new(), Vec::new());
    };

    let Some(injections) = collect_all_injections(&tree.root_node(), text, Some(&injection_query))
    else {
        return (Vec::new(), Vec::new());
    };

    let mut contexts = Vec::with_capacity(injections.len());
    let mut exclusion_ranges = Vec::with_capacity(injections.len());

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
            && parse_offset_directive_for_pattern(&injection_query, injection.pattern_index)
                .is_none()
        {
            combined_groups
                .entry((injection.language.clone(), injection.pattern_index))
                .or_default()
                .push(injection);
        } else {
            singles.push(injection);
        }
    }

    for injection in singles {
        let start = injection.content_node.start_byte();
        let end = injection.content_node.end_byte();

        // Validate bounds
        if start > end || end > text.len() {
            continue;
        }

        // Extract injection content for language detection
        let injection_content = &text[start..end];

        // Resolve injection language
        let Some((resolved_lang, _)) =
            coordinator.resolve_injection_language(&injection.language, injection_content)
        else {
            continue;
        };

        // Get highlight query for resolved language
        let Some(highlight_query) = coordinator.highlight_query(&resolved_lang) else {
            continue;
        };

        // Offset directive resolved at collection time (single source of truth
        // with the bridge path, which applies it in from_region_info)
        let offset = injection.offset;

        // Calculate effective content range
        let content_node = injection.content_node;
        let (inj_start_byte, inj_end_byte) = if let Some(off) = offset {
            use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
            use crate::language::injection::{ceil_char_boundary, floor_char_boundary};
            let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
            let effective = calculate_effective_range(text, byte_range, off);
            // Column deltas are byte counts; a misconfigured query could land
            // inside a multi-byte character. Snap inward so the content slice
            // below cannot panic, and normalize so a degenerate range (both
            // ends inside one codepoint) becomes empty rather than inverted —
            // same guards as from_region_info.
            let start = ceil_char_boundary(text, effective.start);
            let end = floor_char_boundary(text, effective.end);
            (start.min(end), end)
        } else {
            (content_node.start_byte(), content_node.end_byte())
        };

        // Validate effective range after offset adjustment
        if inj_start_byte > inj_end_byte || inj_end_byte > text.len() {
            continue;
        }

        // Compute included ranges for the injection parser (Problem 1: blockquote prefixes).
        // When include_children is false and content_node has named children
        // (e.g., block_continuation), we compute gap ranges so the injection parser
        // only sees actual code content.
        //
        // Disabled when an offset directive is active: compute_included_ranges()
        // returns ranges relative to content_node.start_byte(), but content_text
        // starts at inj_start_byte (offset-adjusted). Passing misaligned ranges
        // to the parser would produce incorrect results.
        let included_ranges = if offset.is_some() {
            None
        } else {
            let from_children =
                compute_included_ranges(&injection.content_node, injection.include_children);
            let from_parent = parent_included_ranges.and_then(|parent_ranges| {
                sub_select_included_ranges(parent_ranges, inj_start_byte, inj_end_byte)
            });
            match (from_children, from_parent) {
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
            }
        };

        // Record exclusion ranges for parent token suppression (Problem 2: host token leaking).
        // When we have per-gap included ranges, push EACH gap as a separate exclusion entry
        // so that compute_active_injection_regions() produces per-line ActiveInjectionBounds values.
        // Otherwise, push the single full content range as before.
        if let Some(ref ranges) = included_ranges {
            // compute_included_ranges returns ranges relative to content_node.start_byte().
            // In this branch, offset.is_none(), so inj_start_byte == content_node.start_byte();
            // we use content_node.start_byte() explicitly as the base for clarity.
            let base_byte = content_node.start_byte();
            for r in ranges {
                let abs_start = base_byte + r.start_byte;
                let abs_end = base_byte + r.end_byte;
                exclusion_ranges.push((abs_start, abs_end));
            }
        } else {
            exclusion_ranges.push((inj_start_byte, inj_end_byte));
        }

        // Derive per-line byte prefix widths from included_ranges.
        // Each range's start_point.column tells us how many bytes of prefix
        // (e.g., "> ") precede the actual content on that line.
        let prefix_byte_widths = match &included_ranges {
            Some(ranges) => derive_prefix_byte_widths(ranges),
            None => Vec::new(),
        };

        contexts.push(InjectionContext {
            resolved_lang,
            highlight_query,
            content_text: &text[inj_start_byte..inj_end_byte],
            host_start_byte: content_start_byte + inj_start_byte,
            included_ranges,
            prefix_byte_widths,
            combined: false,
        });
    }

    for (_group_key, regions) in combined_groups {
        if let Some(ctx) = build_combined_context(
            &regions,
            text,
            coordinator,
            content_start_byte,
            parent_included_ranges,
            &mut exclusion_ranges,
        ) {
            contexts.push(ctx);
        }
    }

    (contexts, exclusion_ranges)
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

/// Build one [`InjectionContext`] covering every region of an
/// `injection.combined` group (#187): the content slice spans from the first
/// block's start to the last block's end, and `included_ranges` restricts the
/// parser to each block, so cross-block context survives (e.g. an HTML tag
/// opened in one `html_block` and closed in another) while the host text
/// between blocks stays invisible to the injected parser.
///
/// Returns `None` when the group's language/query doesn't resolve or every
/// range is clipped away by the parent's exclusions — the regions then simply
/// produce no tokens, like any unresolvable injection.
fn build_combined_context<'a>(
    regions: &[InjectionRegionInfo<'_>],
    text: &'a str,
    coordinator: &LanguageCoordinator,
    content_start_byte: usize,
    parent_included_ranges: Option<&[tree_sitter::Range]>,
    exclusion_ranges: &mut Vec<(usize, usize)>,
) -> Option<InjectionContext<'a>> {
    // collect_all_injections sorts by start byte, so `first` anchors the group.
    let first = regions.first()?;
    let group_start = first.content_node.start_byte();
    let group_start_pos = first.content_node.start_position();
    let group_end = regions.iter().map(|r| r.content_node.end_byte()).max()?;
    if group_start >= group_end || group_end > text.len() {
        return None;
    }

    // Resolve the language once from the first block's content — the grouping
    // key guarantees every region shares the raw injection language.
    let first_content = &text[first.content_node.start_byte()..first.content_node.end_byte()];
    let (resolved_lang, _) =
        coordinator.resolve_injection_language(&first.language, first_content)?;
    let highlight_query = coordinator.highlight_query(&resolved_lang)?;

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
                return None;
            }
            intersected
        }
        None => group_ranges,
    };

    // Suppress this layer's parent tokens within every combined block.
    for r in &included_ranges {
        exclusion_ranges.push((group_start + r.start_byte, group_start + r.end_byte));
    }

    let prefix_byte_widths = derive_prefix_byte_widths(&included_ranges);

    Some(InjectionContext {
        resolved_lang,
        highlight_query,
        content_text: &text[group_start..group_end],
        host_start_byte: content_start_byte + group_start,
        included_ranges: Some(included_ranges),
        prefix_byte_widths,
        combined: true,
    })
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
) -> (Vec<RawToken>, Vec<ActiveInjectionBounds>) {
    use rayon::prelude::*;

    // Collect top-level injection contexts and their byte ranges
    let (contexts, exclusion_byte_ranges) =
        collect_injection_contexts_sync(host_text, host_tree, host_filetype, coordinator, 0, None);

    if contexts.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Create factory from coordinator's registry (cloned for thread safety)
    let factory = ThreadLocalParserFactory::new(coordinator.language_registry_for_parallel());

    // Threshold for parallel processing - below this, Rayon scheduling overhead exceeds benefit
    const PARALLEL_THRESHOLD: usize = 4;

    // Process injections: parallel for larger collections, sequential for small ones
    let mut all_tokens: Vec<RawToken> = if contexts.len() >= PARALLEL_THRESHOLD {
        // Parallel processing for larger collections
        contexts
            .par_iter()
            .flat_map(|ctx| {
                process_injection_sync(
                    ctx,
                    &factory,
                    coordinator,
                    capture_mappings,
                    host_text,
                    host_lines,
                    host_line_starts,
                    1, // depth 1 (first level of injection, host is 0)
                    supports_multiline,
                )
            })
            .collect()
    } else {
        // Sequential processing for small collections to avoid Rayon overhead
        contexts
            .iter()
            .flat_map(|ctx| {
                process_injection_sync(
                    ctx,
                    &factory,
                    coordinator,
                    capture_mappings,
                    host_text,
                    host_lines,
                    host_line_starts,
                    1,
                    supports_multiline,
                )
            })
            .collect()
    };

    // Sort tokens by position (line, then column)
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
        );

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
        );

        assert!(
            tokens.is_empty(),
            "Should return empty at MAX_INJECTION_DEPTH"
        );
    }

    /// Returns the search path for tree-sitter grammars.
    fn test_search_path() -> String {
        std::env::var("TREE_SITTER_GRAMMARS").unwrap_or_else(|_| "deps/tree-sitter".to_string())
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

        let (contexts, exclusions) = collect_injection_contexts_sync(
            COMBINED_LUA_DOC,
            &tree,
            Some("markdown"),
            &coordinator,
            0,
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
}
