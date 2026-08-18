//! Discovering injection regions in a parse tree and resolving them into
//! cacheable, virtual-document-ready form for the LSP bridge.

use std::ops::Range;

use tree_sitter::{Node, Query, QueryCapture, QueryCursor, QueryMatch, StreamingIterator, Tree};
use ulid::Ulid;
use url::Url;

use super::content::{compute_line_column_offsets, extract_clean_content};
use super::language::extract_injection_language;
use super::offset::InjectionOffset;
use super::ranges::{
    compute_included_ranges, compute_included_ranges_clipped, has_combined_for_pattern,
    has_include_children_for_pattern,
};
use crate::language::LanguageCoordinator;
use crate::language::node_tracker::NodeTracker;
use crate::language::query_predicates::check_match_predicates;
use crate::text::{ceil_char_boundary, clamped_slice, floor_char_boundary, fnv1a_hash};

// Keep bridge-region identities in a namespace disjoint from real parse-tree
// injection depths (0..=MAX_INJECTION_DEPTH). The per-range slot then gives
// alternate language layers at identical host coordinates distinct ULIDs.
pub(crate) const REGION_IDENTITY_LAYER_BASE: usize = usize::MAX / 2 + 1;

/// Iterates over the `@injection.content` captures in a query match.
fn iter_injection_content_captures<'a, 'b>(
    match_: &'b QueryMatch<'_, 'a>,
    query: &'b Query,
) -> impl Iterator<Item = QueryCapture<'a>> + 'b {
    match_.captures.iter().copied().filter(|capture| {
        query
            .capture_names()
            .get(capture.index as usize)
            .is_some_and(|name| *name == "injection.content")
    })
}

fn runtime_offset_for_capture(
    query: &Query,
    match_: &QueryMatch<'_, '_>,
    capture: QueryCapture<'_>,
    text: &str,
) -> Option<InjectionOffset> {
    let range = crate::language::query_directives::capture_range(
        query,
        match_,
        capture.index,
        capture.node,
        text,
    );
    let node = capture.node;
    let raw = (
        node.start_byte(),
        node.end_byte(),
        node.start_position(),
        node.end_position(),
    );
    if (
        range.start_byte,
        range.end_byte,
        range.start_point,
        range.end_point,
    ) == raw
    {
        return None;
    }
    let delta = |adjusted: usize, original: usize| {
        let value = adjusted as i128 - original as i128;
        i32::try_from(value).ok()
    };
    Some(InjectionOffset {
        start_row: delta(range.start_point.row, raw.2.row)?,
        start_column: delta(range.start_point.column, raw.2.column)?,
        end_row: delta(range.end_point.row, raw.3.row)?,
        end_column: delta(range.end_point.column, raw.3.column)?,
    })
}

/// Checks if a node is within the bounds of another node
#[cfg(test)]
fn is_node_within(node: &Node, container: &Node) -> bool {
    node.start_byte() >= container.start_byte() && node.end_byte() <= container.end_byte()
}

/// Represents an injection region found in the document
#[derive(Debug, Clone)]
pub(crate) struct InjectionRegionInfo<'a> {
    /// The injection language (e.g., "lua", "yaml")
    pub language: String,
    /// The content node from the injection query
    pub content_node: Node<'a>,
    /// The pattern index (for offset directive lookups)
    pub pattern_index: usize,
    /// Whether `#set! injection.include-children` is set for this pattern.
    /// When true, the injection parser sees the full content node (including named children).
    /// When false, named children (e.g., `block_continuation`) should be excluded.
    pub include_children: bool,
    /// The effective `#offset!` or `#trim!` range encoded as boundary deltas
    /// and resolved at collection time
    /// (the query goes out of scope before consumers like
    /// `CacheableInjectionRegion::from_region_info` run). `None` when the
    /// runtime directives leave the raw range unchanged.
    pub offset: Option<InjectionOffset>,
    /// Whether this pattern's captures form one virtual document.
    pub combined: bool,
    /// Stable query pattern index used as part of tracker identity.
    pub identity_slot: usize,
}

/// Owned injection region for caching (no lifetime dependency on parse tree)
///
/// Unlike `InjectionRegionInfo<'a>`, this struct owns all its data and can be
/// stored in caches that outlive the parse tree. Created via `from_region_info()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CacheableInjectionRegion {
    /// The injection language (e.g., "lua", "yaml")
    pub language: String,
    /// Byte range of the injection content in the source
    pub byte_range: Range<usize>,
    /// Line range (0-indexed, inclusive start, exclusive end)
    pub line_range: Range<u32>,
    /// Column offset of the injection content start within its start line,
    /// stored as UTF-16 code units (matching LSP's position encoding).
    /// Non-zero for inline injections (e.g., code within a markdown paragraph).
    /// Only affects the first line (line 0) of the virtual document during
    /// coordinate translation; subsequent lines start at column 0.
    pub start_column: u32,
    /// Unique identifier for associating with cached tokens.
    /// Generated by NodeTracker as a position-based ULID.
    pub region_id: String,
    /// Hash of the injection content for cache invalidation.
    /// Used to detect when cached semantic tokens should be invalidated:
    /// if content_hash changes (or language changes), the cached tokens are stale.
    pub content_hash: u64,
}

/// Compute the (row, UTF-16 column) of `byte_pos`, using a nearby byte with a
/// known row as an anchor: the row counts newlines only in the span between
/// them (offset deltas are typically a few lines, never the whole document).
/// The column additionally scans back from `byte_pos` to its line start —
/// bounded by the current line's length, or by `byte_pos` itself in a
/// document without newlines.
fn position_of_byte(
    text: &str,
    byte_pos: usize,
    anchor_byte: usize,
    anchor_row: usize,
) -> (u32, u32) {
    // Snap both offsets to in-bounds char boundaries: on a stale tree they can
    // be out of range or mid-codepoint, which would panic the slices below.
    let byte_pos = floor_char_boundary(text, byte_pos);
    let anchor_byte = floor_char_boundary(text, anchor_byte);
    let (lo, hi) = (byte_pos.min(anchor_byte), byte_pos.max(anchor_byte));
    let newlines = text[lo..hi].bytes().filter(|&b| b == b'\n').count();
    let row = if byte_pos >= anchor_byte {
        anchor_row + newlines
    } else {
        // Defensive: a stale tree whose row metadata disagrees with `text`
        // must not panic the server on underflow.
        anchor_row.saturating_sub(newlines)
    };
    let line_start = text[..byte_pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let column = text[line_start..byte_pos].encode_utf16().count() as u32;
    (row as u32, column)
}

/// The effective runtime-directive byte span of `info`'s content node within
/// `text`, snapped to in-bounds UTF-8 char boundaries.
///
/// `#offset!` and `#trim!` narrow (or widen) the raw `@injection.content` span to the
/// bytes the injection parser actually sees — trimming frontmatter `---`
/// fences, string quotes, and the like. The request-routing paths that must
/// agree on *where the injected content is* share this one helper rather than
/// each deriving it: region lookup ([`find_injection_at_position`]), region
/// resolution ([`CacheableInjectionRegion::from_region_info`]), and the native
/// lexical layer's containment filter (`native_bindings`). The semantic,
/// selection-range, and `kakehashi/node` paths still call
/// `calculate_effective_range` themselves — each layers its own gap handling
/// on top, so they are not folded in here.
///
/// Scope: this applies range directives only. Child-exclusion gaps (blockquote `> `
/// prefixes, excluded named children) are *not* subtracted here, because the
/// bridge's coordinate translation models a region as one contiguous
/// `byte_range` plus per-line column offsets — a lookup that rejected gap
/// bytes while translation still mapped them would only trade an
/// over-permissive class for a mistranslating one. Gap membership is tracked
/// separately in #996 item 6, together with the translation model it needs.
/// (`kakehashi/node`'s `injection_stack` does subtract gaps, because it parses
/// with real `included_ranges` and has no such flat mapping.)
///
/// Cheap enough for the per-keystroke lookup path. Without a directive the
/// offset branch is skipped entirely and the boundary snaps iterate at most
/// three bytes. With one, the cost is `calculate_effective_range`'s: O(1) for a
/// column-only directive, and for a row-shaped one (markdown frontmatter's
/// `1 0 -1 0`) a walk between the node edge and the target row — bounded by the
/// directive's row distance, never the document. Either way it is dwarfed by
/// the `collect_all_injections` tree walk the caller runs first.
pub(crate) fn effective_content_range(info: &InjectionRegionInfo<'_>, text: &str) -> Range<usize> {
    let node = &info.content_node;
    let (start, end) = match info.offset {
        Some(offset) => {
            use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
            // calculate_effective_range clamps, snaps inward to char
            // boundaries, and normalizes start <= end, so callers slicing with
            // the result cannot panic.
            let effective = calculate_effective_range(
                text,
                ByteRange::new(node.start_byte(), node.end_byte()),
                offset,
            );
            (effective.start, effective.end)
        }
        None => (node.start_byte(), node.end_byte()),
    };

    // Snap to valid in-bounds char boundaries (ceil start / floor end) so the
    // range is always safe to slice — a stale node can't leave an
    // out-of-bounds range for downstream consumers.
    let start = ceil_char_boundary(text, start);
    let end = floor_char_boundary(text, end).max(start);
    start..end
}

/// Compute clean virtual content and per-line column offsets for an injection region.
///
/// This combines `compute_included_ranges`, `extract_clean_content`, and
/// `compute_line_column_offsets` into a single call, avoiding duplication
/// across resolve methods.
fn extract_virtual_content_and_offsets(
    region: &InjectionRegionInfo,
    cacheable: &CacheableInjectionRegion,
    text: &str,
) -> (String, Vec<u32>) {
    // Child-exclusion gaps restricted to the effective window (#186):
    // cacheable.byte_range already reflects any #offset! directive (applied
    // and char-boundary-aligned by from_region_info), so gaps are clipped to
    // it and relativized to its start. Without an offset the window equals
    // the content node span, so the node-anchored variant gives the same
    // result while reusing tree-sitter's cached node Points (no text scan).
    let included_ranges = if region.offset.is_some() {
        compute_included_ranges_clipped(
            &region.content_node,
            region.include_children,
            text,
            cacheable.byte_range.clone(),
        )
    } else {
        compute_included_ranges(&region.content_node, region.include_children)
    };
    let virtual_content = extract_clean_content(
        text,
        cacheable.byte_range.clone(),
        included_ranges.as_deref(),
    );
    let line_column_offsets = compute_line_column_offsets(
        text,
        cacheable.byte_range.clone(),
        cacheable.start_column,
        included_ranges.as_deref(),
    );
    (virtual_content, line_column_offsets)
}

impl CacheableInjectionRegion {
    /// Create from an InjectionRegionInfo, extracting position data from the node.
    ///
    /// # Panics (debug builds)
    /// Panics if the content node is zero-width (`start_byte >= end_byte`).
    /// Callers must filter zero-width nodes upstream (see `collect_all_injections`).
    pub(crate) fn from_region_info(
        info: &InjectionRegionInfo<'_>,
        region_id: &str,
        text: &str,
    ) -> Self {
        let node = &info.content_node;
        debug_assert!(
            node.start_byte() < node.end_byte(),
            "from_region_info called with zero-width node at byte {}",
            node.start_byte(),
        );

        // #offset! narrows the raw node span to the effective content range
        // (e.g. trimming `---` frontmatter delimiters). The bridge consumes
        // byte_range / line_range / start_column for virtual-document content
        // extraction and coordinate translation, so all of them must reflect
        // the effective range, mirroring the semantic path (#183). The shared
        // helper also boundary-snaps, so the `content` slice below can't panic
        // on a stale node; a stale node then stops matching `node.start_byte()`
        // and routes through the safe `position_of_byte` path.
        let Range {
            start: start_byte,
            end: end_byte,
        } = effective_content_range(info, text);
        let content = &text[start_byte..end_byte];

        // Convert tree-sitter byte column to UTF-16 code units for LSP compatibility.
        // Tree-sitter reports columns as byte offsets, but LSP positions use UTF-16.
        // For ASCII they're identical, but non-ASCII prefixes (CJK, emoji) differ.
        // tree-sitter's `column` is a byte offset from the line start, so
        // `start_byte() - column` correctly recovers the line-start byte even
        // when the line contains multi-byte UTF-8 characters before the node.
        let (start_line, start_column) = if start_byte == node.start_byte() {
            let byte_column = node.start_position().column;
            let line_start_byte = node.start_byte().saturating_sub(byte_column);
            let line_prefix = clamped_slice(text, line_start_byte..node.start_byte());
            (
                node.start_position().row as u32,
                line_prefix.encode_utf16().count() as u32,
            )
        } else {
            position_of_byte(
                text,
                start_byte,
                node.start_byte(),
                node.start_position().row,
            )
        };

        // end_position() points past the last byte. When that position is mid-line
        // (column > 0), the node still occupies that row → exclusive end is row + 1.
        // When column == 0 (node ended with newline), the row is already one past
        // the last occupied line — use it directly.
        let end_line = if end_byte == node.end_byte() {
            let end_pos = node.end_position();
            if end_pos.column > 0 {
                end_pos.row as u32 + 1
            } else {
                end_pos.row as u32
            }
        } else {
            let (end_row, end_column) =
                position_of_byte(text, end_byte, node.end_byte(), node.end_position().row);
            if end_column > 0 { end_row + 1 } else { end_row }
        };

        Self {
            language: info.language.clone(),
            byte_range: start_byte..end_byte,
            line_range: start_line..end_line,
            start_column,
            region_id: region_id.to_string(),
            content_hash: Self::hash_content(content),
        }
    }

    /// Compute a simple hash of content bytes for stable matching.
    fn hash_content(content: &str) -> u64 {
        fnv1a_hash(content)
    }
}

/// Collects all injection regions in the document.
///
/// Unlike `detect_injection` (which requires a specific node), this finds ALL
/// injection regions in the whole document — used by semantic tokens to highlight
/// every injected region. `None` when there is no query.
pub(crate) fn collect_all_injections<'a>(
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
) -> Option<Vec<InjectionRegionInfo<'a>>> {
    collect_all_injections_cancellable(root, text, injection_query, None)
}

/// [`collect_all_injections`] with cooperative cancellation for a document
/// version that became obsolete while the query cursor was walking matches.
pub(crate) fn collect_all_injections_cancellable<'a>(
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
    cancel: Option<&crate::cancel::CancelToken>,
) -> Option<Vec<InjectionRegionInfo<'a>>> {
    if crate::cancel::is_cancelled(cancel) {
        return None;
    }
    let query = injection_query?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, text.as_bytes());

    // Deduplicate repeated matches of the same language layer while preserving
    // distinct languages assigned to the same content range (#598).
    let mut injections_map = std::collections::HashMap::new();
    let mut work_items = 0;

    while let Some(match_) = matches.next() {
        if crate::cancel::is_cancelled_periodically(cancel, &mut work_items) {
            return None;
        }
        if !check_match_predicates(query, match_, text) {
            continue;
        }
        let Some(language) = extract_injection_language(query, match_, text) else {
            continue;
        };
        for capture in iter_injection_content_captures(match_, query) {
            if crate::cancel::is_cancelled_periodically(cancel, &mut work_items) {
                return None;
            }
            if capture.node.start_byte() >= capture.node.end_byte() {
                continue;
            }
            let key = (
                capture.node.start_byte(),
                capture.node.end_byte(),
                language.clone(),
                match_.pattern_index,
            );
            // `or_insert_with` so the per-pattern predicate scans
            // (`has_include_children_for_pattern` / `effective_offset_for_pattern`)
            // are skipped when this content-node range was already inserted by
            // an earlier matching pattern for the same language. Same
            // resulting language layers, fewer scans.
            injections_map.entry(key).or_insert_with(|| {
                let offset = runtime_offset_for_capture(query, match_, capture, text);
                InjectionRegionInfo {
                    language: language.clone(),
                    content_node: capture.node,
                    pattern_index: match_.pattern_index,
                    include_children: has_include_children_for_pattern(query, match_.pattern_index),
                    // Match the semantic path: effective offsets and
                    // multi-region grouping do not compose safely.
                    combined: has_combined_for_pattern(query, match_.pattern_index)
                        && offset.is_none(),
                    identity_slot: 0,
                    offset,
                }
            });
        }
    }

    if crate::cancel::is_cancelled(cancel) {
        return None;
    }

    // Sort by start_byte (primary) and end_byte (secondary) to ensure deterministic ordering
    let mut injections: Vec<_> = injections_map.into_values().collect();
    let adjusted_groups: std::collections::HashSet<_> = injections
        .iter()
        .filter(|region| region.offset.is_some())
        .map(|region| (region.language.clone(), region.pattern_index))
        .collect();
    for region in &mut injections {
        if adjusted_groups.contains(&(region.language.clone(), region.pattern_index)) {
            region.combined = false;
        }
    }
    injections.sort_by(|a, b| {
        (
            a.content_node.start_byte(),
            a.content_node.end_byte(),
            a.pattern_index,
            a.language.as_str(),
        )
            .cmp(&(
                b.content_node.start_byte(),
                b.content_node.end_byte(),
                b.pattern_index,
                b.language.as_str(),
            ))
    });
    for region in &mut injections {
        // Retain the stable query-position component for discovery tests. The
        // collision-free language component is allocated later by
        // `NodeTracker::named_layer_for_incarnation`, where URI lifecycle state
        // can own and reclaim dynamic language names.
        region.identity_slot = region.pattern_index;
    }
    Some(injections)
}

/// Detects injection and returns both the language and the content node
/// Also returns the pattern index of the innermost injection for offset lookups
pub(crate) fn detect_injection<'a>(
    node: &Node<'a>,
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
    base_language: &str,
) -> Option<(Vec<String>, Node<'a>, usize, Option<InjectionOffset>)> {
    let injections = collect_injection_regions(node, root, text, injection_query)?;

    if injections.is_empty() {
        return None;
    }

    // Sort injections by their range (outer to inner)
    let mut sorted_injections = injections;
    sorted_injections.sort_by(|a, b| {
        // Sort by start byte (ascending), then by end byte (descending)
        // This ensures outer injections come before inner ones. Same-range
        // alternate-language layers tie-break by (pattern_index, language) —
        // the discovery sorts' exact order — so the hierarchy and the
        // innermost pick stay deterministic regardless of input order.
        a.start_byte
            .cmp(&b.start_byte)
            .then(b.end_byte.cmp(&a.end_byte))
            .then(a.pattern_index.cmp(&b.pattern_index))
            .then(a.language.as_str().cmp(b.language.as_str()))
    });

    // Equal spans are alternative interpretations, not nested syntax. Keep
    // the first query-pattern candidate for single-hierarchy consumers (the
    // same priority used by bridge position resolution) while preserving real
    // geometric nesting across different spans.
    let mut previous_range = None;
    sorted_injections.retain(|region| {
        let range = (region.start_byte, region.end_byte);
        if previous_range == Some(range) {
            false
        } else {
            previous_range = Some(range);
            true
        }
    });

    // Build the language hierarchy from outermost to innermost
    let mut hierarchy = vec![base_language.to_string()];
    for region in &sorted_injections {
        hierarchy.push(region.language.clone());
    }

    // Return the innermost content node and its pattern index
    let innermost = sorted_injections.last()?;

    Some((
        hierarchy,
        innermost.content_node,
        innermost.pattern_index,
        innermost.offset,
    ))
}

/// Represents an injection region with its metadata (unresolved intermediate data).
struct RawInjectionRegion<'a> {
    start_byte: usize,
    end_byte: usize,
    language: String,
    content_node: Node<'a>,
    pattern_index: usize,
    offset: Option<InjectionOffset>,
}

/// Collects all injection regions that contain the given node
/// Returns a list of `RawInjectionRegion` values for each injection containing the node.
fn collect_injection_regions<'a>(
    node: &Node<'a>,
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
) -> Option<Vec<RawInjectionRegion<'a>>> {
    let query = injection_query?;

    // Run the query on the entire tree
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, text.as_bytes());

    // Collect all injection regions that contain our node
    // Deduplicate by node range and language so alternate language layers on
    // the same node remain discoverable.
    let mut injections_map = std::collections::HashMap::new();

    while let Some(match_) = matches.next() {
        if let Some((content_node, language, pattern_index, offset, start_byte, end_byte)) =
            extract_content_and_language(node, match_, query, text)
        {
            let key = (start_byte, end_byte, language.clone(), pattern_index);

            // Keep the first match for each range/language pair; distinct
            // languages on the same range remain separate layers.
            injections_map.entry(key).or_insert(RawInjectionRegion {
                start_byte,
                end_byte,
                language,
                content_node,
                pattern_index,
                offset,
            });
        }
    }

    let mut injections: Vec<_> = injections_map.into_values().collect();
    injections.sort_by(|a, b| {
        (
            a.start_byte,
            std::cmp::Reverse(a.end_byte),
            a.pattern_index,
            a.language.as_str(),
        )
            .cmp(&(
                b.start_byte,
                std::cmp::Reverse(b.end_byte),
                b.pattern_index,
                b.language.as_str(),
            ))
    });

    Some(injections)
}

/// Extracts the injection content node and language if the given node is within it
/// Also returns the pattern index for offset lookups
fn extract_content_and_language<'a>(
    node: &Node<'a>,
    match_: &QueryMatch<'_, 'a>,
    query: &Query,
    text: &str,
) -> Option<(
    Node<'a>,
    String,
    usize,
    Option<InjectionOffset>,
    usize,
    usize,
)> {
    for capture in iter_injection_content_captures(match_, query) {
        let content_node = capture.node;

        if !check_match_predicates(query, match_, text) {
            return None;
        }
        let offset = runtime_offset_for_capture(query, match_, capture, text);
        let effective = if let Some(offset) = offset {
            use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
            let range = calculate_effective_range(
                text,
                ByteRange::new(content_node.start_byte(), content_node.end_byte()),
                offset,
            );
            range.start..range.end
        } else {
            content_node.byte_range()
        };
        if node.start_byte() >= effective.start
            && node.end_byte() <= effective.end
            && let Some(language) = extract_injection_language(query, match_, text)
        {
            return Some((
                content_node,
                language,
                match_.pattern_index,
                offset,
                effective.start,
                effective.end,
            ));
        }
    }

    None
}

/// Find the injection region at the given byte offset under `boundary`'s
/// end-boundary rule: half-open containment, optionally falling back to a
/// region whose trailing edge the offset sits on (see [`RegionBoundary`]).
///
/// Containment is judged on each region's **effective** span — the raw
/// `@injection.content` node after its `#offset!` directive
/// ([`effective_content_range`]) — which is the same span the resolved region
/// and every coordinate translation downstream are built from. Judging it on
/// the raw node instead would let a caret on bytes the directive trimmed
/// (a frontmatter `---` fence, a string's quotes) select a region that does
/// not actually contain it, and would leave bytes a directive *added* past
/// the raw node unreachable (#996 item 1).
///
/// Regions are sorted by query pattern index, so same-range alternate
/// languages use explicit query-order priority for single-result bridge APIs.
/// Whole-document and hierarchy discovery still retain every language layer.
///
/// Returns `(index, region)` for use with `calculate_region_id`, or `None`
/// when no region matches under the boundary rule.
fn find_injection_at_position<'a>(
    injections: &'a [InjectionRegionInfo<'a>],
    byte_offset: usize,
    text: &str,
    boundary: RegionBoundary,
) -> Option<(usize, &'a InjectionRegionInfo<'a>)> {
    let doc_len = text.len();
    let half_open = injections.iter().enumerate().find(|(_, inj)| {
        let range = effective_content_range(inj, text);
        byte_offset >= range.start && byte_offset < range.end
    });
    match boundary {
        RegionBoundary::HalfOpen => half_open,
        RegionBoundary::CaretEndFallback => half_open.or_else(|| {
            // No region contains the byte: accept a region whose trailing edge
            // the caret sits on, provided that edge is mid-line — or at the
            // document's end, where a column-0 edge means an unclosed block
            // whose content ends on the newline just typed, not a closing
            // fence (see the variant doc). Same iteration order as above —
            // `collect_all_injections`'s document order, sorted by RAW span —
            // so regions ending at the same byte resolve to the first of them.
            // That is the outermost region only while no directive shifts a
            // span: raw order tracks effective nesting exactly when the
            // effective spans equal the raw ones. The half-open scan above
            // shares that property, and same-range alternate languages depend
            // on the raw/pattern order for their documented query-order
            // priority, so both scans keep it.
            //
            // The second scan runs only on the miss path, over the handful of
            // regions a document has. It recomputes each effective range
            // rather than caching the first scan's: with no `#offset!` that is
            // two boundary snaps, and with one it is a walk bounded by the
            // directive's row distance — cheaper than allocating a per-region
            // range vector on every lookup, hit path included. Both scans are
            // dwarfed by the `collect_all_injections` tree walk that precedes
            // them.
            injections.iter().enumerate().find(|(_, inj)| {
                let range = effective_content_range(inj, text);
                // No `start < end` condition: a region an `#offset!` collapses
                // to zero width is routable at the byte it collapses to, on
                // the same terms as any other region — that position IS the
                // whole (empty) injection, and its virtual document is empty,
                // so the caret maps to a valid (0, 0). Half-open declines it
                // by arithmetic alone, which is right: no character to hover.
                //
                // Zero width buys no exemption from the column-0 rule below.
                // A collapse onto a line start is the closing-fence shape (an
                // EMPTY frontmatter collapses exactly there) and stays
                // outside; a collapse mid-line — `html!{}` under the bundled
                // rust `0 1 0 -1` — routes.
                range.end == byte_offset
                    && (ends_mid_line(text, range.end) || byte_offset == doc_len)
            })
        }),
    }
}

/// Whether the byte offset `end` sits mid-line — i.e. tree-sitter would report
/// a non-zero column there.
///
/// Applies tree-sitter's column rule (a column counted from the last `\n`) to
/// `text`, rather than the LSP one, so it stays consistent with the
/// coordinate-translation pipeline, which shares that convention. The
/// divergence that follows — a region ending right after a LONE `\r`, an LSP
/// line break but not a tree-sitter one, reads as mid-line — is preserved on
/// purpose and tracked as #996 item 4; fixing it here alone would only split
/// the lookup from translation.
///
/// Reads `text` rather than the node's parse-time `end_position()`, because an
/// `#offset!`-shifted boundary has no tree-sitter `Point` to consult. On a
/// stale tree the two can therefore disagree — this answer is the better one,
/// being derived from the text actually being served.
///
/// Callers must pass an in-bounds `end`; a value past `text.len()` reads as
/// column 0. The `end > 0` guard keeps the function total — without it
/// `end - 1` underflows — and `false` is also the right answer at byte 0.
fn ends_mid_line(text: &str, end: usize) -> bool {
    end > 0 && text.as_bytes().get(end - 1).is_some_and(|&b| b != b'\n')
}

/// How [`InjectionResolver::resolve_at_byte_offset`] treats a region's end
/// boundary.
///
/// Node containment is half-open `[start, end)` (node-reference-protocol
/// § Half-Open Intervals), and that stays the default. But caret-shaped
/// methods (the `region_boundary_for_method` set: completion, signatureHelp,
/// linkedEditingRange, onTypeFormatting) fire with the insert-mode caret
/// sitting *after* the last typed byte — for a region that ends mid-line (a
/// vim `!cmd`, an embedded string) that caret is exactly the region's
/// effective end byte, and a strict half-open lookup routes the request away
/// from the injection the user is visibly typing in.
///
/// Both variants measure the effective post-`#offset!` span, never the raw
/// `@injection.content` node — see [`effective_content_range`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RegionBoundary {
    /// Strict half-open `[start, end)`: a cursor at the end byte is outside.
    HalfOpen,
    /// Half-open first; only when that finds nothing, accept a region whose
    /// **effective** (post-`#offset!`) end byte equals the cursor **and** whose
    /// end sits mid-line (a non-zero end column) or at the document's end.
    /// Both conditions are measured on the effective span, so a directive that
    /// trims the end moves the byte this fires at — and one that trims it back
    /// across a `\n` makes it stop firing, the region having become the
    /// column-0 shape below. A region ending at column 0
    /// (fenced-block shape) keeps the caret on the closing fence outside:
    /// every caret position on its last content line is already inside
    /// half-open, so the fallback would only ever add the fence line itself.
    /// The end-of-document case is the one column-0 shape with no fence line
    /// to protect — an unclosed block whose content ends on the newline the
    /// user just typed — and mirrors the node-reference-protocol ADR's
    /// end-of-document exception (`b == L && e == L`).
    ///
    /// A region an `#offset!` collapses to zero width has no interior for
    /// half-open to match, but the caret routes at the byte it collapses to —
    /// that position is the whole (empty) injection — subject to the same
    /// mid-line-or-EOF condition as everything else. A collapse onto a line
    /// start is the closing-fence shape and stays outside.
    CaretEndFallback,
}

/// Resolved injection region with all necessary context for LSP bridge requests
#[derive(Clone)]
pub(crate) struct ResolvedInjection {
    /// Cacheable injection region with line range information
    pub region: CacheableInjectionRegion,
    /// Language of the injection content
    pub injection_language: String,
    /// Extracted virtual document content. Excluded line prefixes are stripped;
    /// combined regions additionally retain host line layout with empty or
    /// whitespace-only gaps.
    pub virtual_content: String,
    /// Per-virtual-line column offsets for coordinate translation.
    /// Each entry is the UTF-16 column offset for that virtual line.
    pub line_column_offsets: Vec<u32>,
    /// Whether `virtual_content` maps through the ordinary single-region path.
    /// For a multi-capture combined document this is `false` whenever the union
    /// of included ranges leaves uncovered host bytes, including inter-capture
    /// gaps and excluded prefix/child bytes. A combined pattern that currently
    /// matches one capture uses the ordinary mapping and remains `true`.
    pub contiguous: bool,
}

/// Central service for resolving injection regions at LSP positions
pub(crate) struct InjectionResolver;

impl InjectionResolver {
    /// Resolve the injection region (if any) covering `byte_offset` — or,
    /// under [`RegionBoundary::CaretEndFallback`], ending at it. Shared by
    /// LSP handlers (hover, completion, definition, …) at a cursor position.
    ///
    /// Does not hold any Document lock: inputs (`tree`, `text`) must be pre-cloned,
    /// typically via `DocumentSnapshot`.
    // Pre-cloned snapshot inputs (no Document lock) plus the boundary rule; a
    // params struct would just relocate the list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_at_byte_offset(
        coordinator: &LanguageCoordinator,
        tracker: &NodeTracker,
        uri: &Url,
        tree: &Tree,
        text: &str,
        injection_query: &Query,
        byte_offset: usize,
        incarnation: u64,
        boundary: RegionBoundary,
    ) -> Option<ResolvedInjection> {
        let injections = collect_all_injections(&tree.root_node(), text, Some(injection_query))?;
        let (_region_index, region) =
            find_injection_at_position(&injections, byte_offset, text, boundary)?;
        if region.combined {
            let group: Vec<_> = injections
                .iter()
                .filter(|candidate| {
                    candidate.combined
                        && candidate.pattern_index == region.pattern_index
                        && candidate.language == region.language
                })
                .collect();
            Self::build_combined_injection(
                coordinator,
                Some((tracker, uri, incarnation)),
                &group,
                None,
                text,
            )
        } else {
            Self::build_resolved_injection(coordinator, tracker, uri, region, text, incarnation)
        }
    }

    /// Calculate a stable ULID-based region_id for an injection.
    ///
    /// Phase 2 (lazy-node-identity-tracking): keyed on position (start_byte,
    /// end_byte, kind, layer), so the ULID stays constant for the same position
    /// and same-range identity slot.
    ///
    /// Region IDs use a reserved tracker-layer namespace plus the deterministic
    /// same-range identity slot. This keeps alternate language/query layers at
    /// identical host coordinates distinct without colliding with real parse
    /// injection depths used by `kakehashi/node`.
    ///
    /// Deliberately keyed on the **raw** `content_node` bytes, not the
    /// effective post-`#offset!` span that [`find_injection_at_position`]
    /// measures. This is a tracker identity key — "which host node is this?" —
    /// not a containment question, and [`Self::resolve_by_region_id`] looks the
    /// region back up by those same raw bytes. Re-keying it on the effective
    /// span would break region-id stability across the mint/resolve pair for
    /// every offset-bearing region.
    pub(crate) fn calculate_region_id(
        tracker: &NodeTracker,
        uri: &Url,
        injection: &InjectionRegionInfo,
        incarnation: u64,
    ) -> Option<Ulid> {
        let identity_layer = Self::region_identity_layer(tracker, uri, injection, incarnation)?;
        tracker.get_or_create_in_layer_for_incarnation(
            uri,
            injection.content_node.start_byte(),
            injection.content_node.end_byte(),
            injection.content_node.kind(),
            identity_layer,
            incarnation,
        )
    }

    /// Tracker-layer key used by both inline and batch region-ID minting. The
    /// slot is collision-free and stable within one URI incarnation; its
    /// numeric value depends on first-observation order.
    pub(crate) fn region_identity_layer(
        tracker: &NodeTracker,
        uri: &Url,
        injection: &InjectionRegionInfo,
        incarnation: u64,
    ) -> Option<usize> {
        let slot = tracker.named_layer_for_incarnation(
            uri,
            injection.pattern_index,
            &injection.language,
            incarnation,
        )?;
        REGION_IDENTITY_LAYER_BASE.checked_add(slot)
    }

    /// Derive a parser-independent canonical injection language for bridge
    /// selection and stable virtual-document identity.
    ///
    /// This derives the stable bridge key from an explicit configured base or
    /// heuristic normalization (for example, `py` to `python`).
    ///
    /// Falls back to the raw identifier when no configured or heuristic
    /// canonical candidate exists; bridge matching then uses that explicit key.
    fn resolve_language(
        coordinator: &LanguageCoordinator,
        raw_identifier: &str,
        content: &str,
    ) -> String {
        coordinator.canonical_injection_language(raw_identifier, content)
    }

    /// Build a [`ResolvedInjection`] from a raw injection region.
    ///
    /// Shared by [`Self::resolve_at_byte_offset`] and [`Self::resolve_all`] to
    /// avoid duplicating the region-id → cacheable-region → virtual-content →
    /// language-resolution pipeline.
    fn build_resolved_injection(
        coordinator: &LanguageCoordinator,
        tracker: &NodeTracker,
        uri: &Url,
        region: &InjectionRegionInfo,
        text: &str,
        incarnation: u64,
    ) -> Option<ResolvedInjection> {
        let region_id = Self::calculate_region_id(tracker, uri, region, incarnation)?;
        let region_id_str = region_id.to_string();
        let cacheable_region =
            CacheableInjectionRegion::from_region_info(region, &region_id_str, text);
        let (virtual_content, line_column_offsets) =
            extract_virtual_content_and_offsets(region, &cacheable_region, text);
        let resolved_language =
            Self::resolve_language(coordinator, &region.language, &virtual_content);
        Some(ResolvedInjection {
            region: cacheable_region,
            injection_language: resolved_language,
            virtual_content,
            line_column_offsets,
            contiguous: true,
        })
    }

    /// Build one bridge virtual document for all captures of an
    /// `injection.combined` pattern. Excluded line prefixes are stripped and
    /// recorded as per-line offsets, while host-only gaps between captures are
    /// represented by empty or whitespace-only lines. The downstream parser
    /// sees one document and retains cross-block context without inheriting
    /// indentation from host prefixes.
    fn build_combined_injection(
        coordinator: &LanguageCoordinator,
        identity: Option<(&NodeTracker, &Url, u64)>,
        regions: &[&InjectionRegionInfo<'_>],
        prebuilt: Option<&[&CacheableInjectionRegion]>,
        text: &str,
    ) -> Option<ResolvedInjection> {
        debug_assert!(!regions.is_empty());
        let owned_cacheable;
        let cacheable: Vec<&CacheableInjectionRegion> = match prebuilt {
            Some(cacheable) => cacheable.to_vec(),
            None => {
                let (tracker, uri, incarnation) =
                    identity.expect("non-prebuilt regions require identity");
                owned_cacheable = regions
                    .iter()
                    .map(|region| {
                        let id = Self::calculate_region_id(tracker, uri, region, incarnation)?;
                        Some(CacheableInjectionRegion::from_region_info(
                            region,
                            &id.to_string(),
                            text,
                        ))
                    })
                    .collect::<Option<Vec<_>>>()?;
                owned_cacheable.iter().collect()
            }
        };
        let first = regions[0];
        let first_cacheable = cacheable[0];
        if regions.len() == 1 {
            let (virtual_content, line_column_offsets) =
                extract_virtual_content_and_offsets(first, first_cacheable, text);
            let injection_language =
                Self::resolve_language(coordinator, &first.language, &virtual_content);
            let mut region = first_cacheable.clone();
            region.content_hash = CacheableInjectionRegion::hash_content(&virtual_content);
            return Some(ResolvedInjection {
                region,
                injection_language,
                virtual_content,
                line_column_offsets,
                contiguous: true,
            });
        }
        let group_start = first_cacheable.byte_range.start;
        let group_end = cacheable
            .iter()
            .map(|region| region.byte_range.end)
            .max()
            .unwrap_or(group_start);

        let mut included = Vec::new();
        for (region, cacheable) in regions.iter().zip(&cacheable) {
            let ranges = if region.offset.is_some() {
                compute_included_ranges_clipped(
                    &region.content_node,
                    region.include_children,
                    text,
                    cacheable.byte_range.clone(),
                )
            } else {
                compute_included_ranges(&region.content_node, region.include_children)
            };
            match ranges {
                Some(ranges) => included.extend(ranges.into_iter().map(|range| {
                    cacheable.byte_range.start + range.start_byte
                        ..cacheable.byte_range.start + range.end_byte
                })),
                None => included.push(cacheable.byte_range.clone()),
            }
        }
        included.sort_by_key(|range| (range.start, range.end));
        let mut covered_until = group_start;
        let contiguous = included.iter().all(|range| {
            if range.start > covered_until {
                return false;
            }
            covered_until = covered_until.max(range.end);
            true
        }) && covered_until >= group_end;
        let (virtual_content, line_column_offsets) =
            build_combined_virtual_content(text, group_start..group_end, &included);

        let mut combined_region = first_cacheable.clone();
        combined_region.byte_range.end = group_end;
        combined_region.line_range.end = cacheable
            .iter()
            .map(|region| region.line_range.end)
            .max()
            .unwrap_or(combined_region.line_range.end);
        combined_region.content_hash = CacheableInjectionRegion::hash_content(&virtual_content);
        let resolved_language =
            Self::resolve_language(coordinator, &first.language, &virtual_content);
        Some(ResolvedInjection {
            region: combined_region,
            injection_language: resolved_language,
            virtual_content,
            line_column_offsets,
            contiguous,
        })
    }

    /// Resolve every bridge virtual document in the host (whole-doc operations
    /// like `documentLink` use this rather than a position lookup). Combined
    /// capture groups produce one resolved document. Holds no
    /// Document lock — `tree`/`text` must already be cloned, typically via
    /// `DocumentSnapshot`. Empty vec when nothing matches.
    pub(crate) fn resolve_all(
        coordinator: &LanguageCoordinator,
        tracker: &NodeTracker,
        uri: &Url,
        tree: &Tree,
        text: &str,
        injection_query: &Query,
        incarnation: u64,
    ) -> Vec<ResolvedInjection> {
        let Some(injections) =
            collect_all_injections(&tree.root_node(), text, Some(injection_query))
        else {
            return Vec::new();
        };

        Self::resolve_from_regions(coordinator, tracker, uri, &injections, text, incarnation)
    }

    /// Resolve the exact bridge layer identified by an opaque region ID.
    ///
    /// A byte lookup alone is ambiguous when multiple query patterns assign
    /// alternate languages or geometry to the same host node.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn resolve_by_region_id(
        coordinator: &LanguageCoordinator,
        tracker: &NodeTracker,
        uri: &Url,
        tree: &Tree,
        text: &str,
        injection_query: &Query,
        region_id: &str,
        incarnation: u64,
    ) -> Option<ResolvedInjection> {
        let ulid = Ulid::from_string(region_id).ok()?;
        let (start, end, kind, identity_layer, tracked_incarnation) =
            tracker.lookup_node(uri, &ulid)?;
        if tracked_incarnation != incarnation {
            return None;
        }

        let regions = collect_all_injections(&tree.root_node(), text, Some(injection_query))?;
        let region = regions.iter().find(|candidate| {
            candidate.content_node.start_byte() == start
                && candidate.content_node.end_byte() == end
                && candidate.content_node.kind() == kind
                && Self::region_identity_layer(tracker, uri, candidate, incarnation)
                    == Some(identity_layer)
        })?;

        if region.combined {
            let group: Vec<_> = regions
                .iter()
                .filter(|candidate| {
                    candidate.combined
                        && candidate.pattern_index == region.pattern_index
                        && candidate.language == region.language
                })
                .collect();
            // Every capture in a combined group has a tracker ID, while the
            // virtual document uses the first capture's ID as its canonical
            // identity. Resolving any member ID must still reach that shared
            // document rather than rejecting non-first captures below.
            Self::build_combined_injection(
                coordinator,
                Some((tracker, uri, incarnation)),
                &group,
                None,
                text,
            )
        } else {
            let resolved = Self::build_resolved_injection(
                coordinator,
                tracker,
                uri,
                region,
                text,
                incarnation,
            )?;
            (resolved.region.region_id == region_id).then_some(resolved)
        }
    }

    /// [`resolve_all`](Self::resolve_all) minus the injection-query run, for a
    /// caller that already collected the regions (the populate pass — never
    /// discover twice, parse-snapshot ADR §3).
    pub(crate) fn resolve_from_regions(
        coordinator: &LanguageCoordinator,
        tracker: &NodeTracker,
        uri: &Url,
        regions: &[InjectionRegionInfo<'_>],
        text: &str,
        incarnation: u64,
    ) -> Vec<ResolvedInjection> {
        Self::resolve_grouped(
            coordinator,
            Some((tracker, uri, incarnation)),
            regions,
            None,
            text,
            None,
        )
        .expect("resolution without cancellation cannot be cancelled")
    }

    /// [`resolve_from_regions`](Self::resolve_from_regions) fed with the
    /// `CacheableInjectionRegion`s the caller already built from the SAME
    /// `regions` (populate's path): skips the duplicate per-region id mint
    /// and content hash — populate runs on the pre-publish critical path,
    /// where repeating work the caller just did delays the settle signal.
    /// `regions` and `cacheable` must be index-aligned (both derive from one
    /// `collect_all_injections` pass).
    pub(crate) fn resolve_from_prebuilt_cancellable(
        coordinator: &LanguageCoordinator,
        regions: &[InjectionRegionInfo<'_>],
        cacheable: &[CacheableInjectionRegion],
        text: &str,
        cancel: Option<&crate::cancel::CancelToken>,
    ) -> Option<Vec<ResolvedInjection>> {
        Self::resolve_grouped(coordinator, None, regions, Some(cacheable), text, cancel)
    }

    fn resolve_grouped(
        coordinator: &LanguageCoordinator,
        identity: Option<(&NodeTracker, &Url, u64)>,
        regions: &[InjectionRegionInfo<'_>],
        prebuilt: Option<&[CacheableInjectionRegion]>,
        text: &str,
        cancel: Option<&crate::cancel::CancelToken>,
    ) -> Option<Vec<ResolvedInjection>> {
        if crate::cancel::is_cancelled(cancel) {
            return None;
        }
        enum Slot {
            Single(usize),
            Combined(Vec<usize>),
        }

        // Partition once in document order. A per-group scan makes dynamic
        // language captures quadratic when nearly every region is its own
        // (language, pattern) group.
        let mut slots = Vec::new();
        let mut combined_slots: std::collections::HashMap<(&str, usize), usize> =
            std::collections::HashMap::new();
        let mut work_items = 0;
        for (index, region) in regions.iter().enumerate() {
            if crate::cancel::is_cancelled_periodically(cancel, &mut work_items) {
                return None;
            }
            if region.combined {
                let key = (region.language.as_str(), region.pattern_index);
                if let Some(&slot_index) = combined_slots.get(&key) {
                    let Slot::Combined(indices) = &mut slots[slot_index] else {
                        unreachable!("combined slot index must identify a combined slot")
                    };
                    indices.push(index);
                } else {
                    combined_slots.insert(key, slots.len());
                    slots.push(Slot::Combined(vec![index]));
                }
            } else {
                slots.push(Slot::Single(index));
            }
        }

        let mut resolved = Vec::with_capacity(slots.len());
        for slot in slots {
            if crate::cancel::is_cancelled_periodically(cancel, &mut work_items) {
                return None;
            }
            let index = match slot {
                Slot::Combined(indices) => {
                    let group: Vec<_> = indices.iter().map(|&i| &regions[i]).collect();
                    let prebuilt_group = prebuilt.map(|cacheable| {
                        indices.iter().map(|&i| &cacheable[i]).collect::<Vec<_>>()
                    });
                    if let Some(combined) = Self::build_combined_injection(
                        coordinator,
                        identity,
                        &group,
                        prebuilt_group.as_deref(),
                        text,
                    ) {
                        resolved.push(combined);
                    }
                    continue;
                }
                Slot::Single(index) => index,
            };
            let region = &regions[index];
            if let Some(cacheable) = prebuilt {
                let (virtual_content, line_column_offsets) =
                    extract_virtual_content_and_offsets(region, &cacheable[index], text);
                let resolved_language =
                    Self::resolve_language(coordinator, &region.language, &virtual_content);
                resolved.push(ResolvedInjection {
                    region: cacheable[index].clone(),
                    injection_language: resolved_language,
                    virtual_content,
                    line_column_offsets,
                    contiguous: true,
                });
            } else {
                let (tracker, uri, incarnation) =
                    identity.expect("non-prebuilt regions require identity");
                if let Some(single) = Self::build_resolved_injection(
                    coordinator,
                    tracker,
                    uri,
                    region,
                    text,
                    incarnation,
                ) {
                    resolved.push(single);
                }
            }
        }
        (!crate::cancel::is_cancelled(cancel)).then_some(resolved)
    }
}

fn mask_outside_ranges(text: &str, span: Range<usize>, included: &[Range<usize>]) -> String {
    // The output is the span verbatim with excluded bytes turned to spaces
    // (multi-byte chars can shrink it, never grow it) — preallocate the span.
    let mut output = String::with_capacity(span.len());
    let mut cursor = span.start;
    for range in included {
        if range.end <= cursor {
            continue;
        }
        if range.start >= span.end {
            break;
        }
        let start = range.start.clamp(cursor, span.end);
        let end = range.end.clamp(start, span.end);
        push_coordinate_whitespace(&mut output, clamped_slice(text, cursor..start));
        output.push_str(clamped_slice(text, start..end));
        cursor = end;
    }
    push_coordinate_whitespace(&mut output, clamped_slice(text, cursor..span.end));
    output
}

/// Build a line-preserving combined document while stripping excluded prefixes.
///
/// Host-only gaps stay as empty or whitespace-only lines so virtual and host
/// line numbers remain aligned. On lines containing injected content, bytes
/// before the first included range are removed and recorded as a UTF-16 column
/// offset, matching the isolated-region `extract_clean_content` contract. Any
/// later gaps on the same line remain coordinate-preserving whitespace because
/// the bridge offset model supports one translation offset per line.
fn build_combined_virtual_content(
    text: &str,
    span: Range<usize>,
    included: &[Range<usize>],
) -> (String, Vec<u32>) {
    // Tree-sitter byte ranges are only valid for the exact parsed text. A
    // stale tree must not turn a combined-document rebuild into an invalid
    // UTF-8 slice or an oversized allocation.
    let span = ceil_char_boundary(text, span.start)..floor_char_boundary(text, span.end);
    if span.start >= span.end {
        return (String::new(), Vec::new());
    }
    let mut output = String::with_capacity(span.len());
    let mut offsets = Vec::new();
    let mut line_start = span.start;
    let mut range_index = 0;
    let mut host_line_start = text[..line_start]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);

    while line_start < span.end {
        let remaining = clamped_slice(text, line_start..span.end);
        let line_end = remaining
            .find('\n')
            .map_or(span.end, |newline| line_start + newline + 1);
        let mut content_end = line_end;
        if text.as_bytes().get(content_end.saturating_sub(1)) == Some(&b'\n') {
            content_end -= 1;
            if text.as_bytes().get(content_end.saturating_sub(1)) == Some(&b'\r') {
                content_end -= 1;
            }
        }

        while included
            .get(range_index)
            .is_some_and(|range| range.end <= line_start)
        {
            range_index += 1;
        }
        let first_included = included.get(range_index).and_then(|range| {
            let start = ceil_char_boundary(text, range.start.max(line_start));
            let end = range.end.min(content_end);
            let includes_line_break =
                content_end < line_end && start == content_end && range.end > content_end;
            (start < end || includes_line_break).then_some(start)
        });

        if let Some(first_included) = first_included {
            offsets.push(
                clamped_slice(text, host_line_start..first_included)
                    .encode_utf16()
                    .count() as u32,
            );
            output.push_str(&mask_outside_ranges(
                text,
                first_included..line_end,
                &included[range_index..],
            ));
        } else {
            offsets.push(0);
            output.push_str(clamped_slice(text, content_end..line_end));
        }

        line_start = line_end;
        host_line_start = line_end;
    }

    if output.ends_with('\n') {
        offsets.push(0);
    }
    (output, offsets)
}

fn push_coordinate_whitespace(output: &mut String, text: &str) {
    for character in text.chars() {
        if matches!(character, '\n' | '\r') {
            output.push(character);
        } else {
            output.extend(std::iter::repeat_n(' ', character.len_utf16()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LanguageCoordinator;
    use crate::language::node_tracker::NodeTracker;
    use rstest::rstest;
    use tree_sitter::{Node, Parser, Query, StreamingIterator};
    use url::Url;

    fn create_rust_parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        parser
    }

    fn parse_rust_code(parser: &mut Parser, code: &str) -> tree_sitter::Tree {
        parser.parse(code, None).expect("parse rust")
    }

    // Helper function to find a node at a specific byte position
    fn find_node_at_byte<'a>(root: &Node<'a>, byte: usize) -> Option<Node<'a>> {
        root.descendant_for_byte_range(byte, byte)
    }

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///test/{}.rs", name)).unwrap()
    }

    fn test_coordinator() -> LanguageCoordinator {
        LanguageCoordinator::new()
    }

    #[test]
    fn canonicalizes_token_independently_of_parser_availability() {
        let coordinator = LanguageCoordinator::new();

        assert_eq!(
            InjectionResolver::resolve_language(&coordinator, "py", "print('hello')"),
            "python"
        );

        coordinator
            .language_registry_for_parallel()
            .register("py".to_string(), tree_sitter_python::LANGUAGE.into());
        assert_eq!(
            InjectionResolver::resolve_language(&coordinator, "py", "print('hello')"),
            "python"
        );
    }

    #[test]
    fn explicit_token_beats_loaded_first_line_language() {
        let coordinator = LanguageCoordinator::new();
        coordinator
            .language_registry_for_parallel()
            .register("lua".to_string(), tree_sitter_lua::LANGUAGE.into());

        assert_eq!(
            InjectionResolver::resolve_language(
                &coordinator,
                "py",
                "#!/usr/bin/env lua\nprint('hello')",
            ),
            "python"
        );
    }

    #[test]
    fn configured_base_beats_syntect_token_normalization_without_parser() {
        let coordinator = LanguageCoordinator::new();
        coordinator.load_settings(&crate::config::WorkspaceSettings {
            languages: std::collections::HashMap::from([(
                "py".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("custom-python".to_string()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });

        assert_eq!(
            InjectionResolver::resolve_language(&coordinator, "py", ""),
            "custom-python"
        );
    }

    #[test]
    fn canonicalizes_first_line_without_loaded_parser() {
        let coordinator = LanguageCoordinator::new();

        assert_eq!(
            InjectionResolver::resolve_language(
                &coordinator,
                "unknown",
                "#!/usr/bin/env python\nprint('hello')",
            ),
            "python"
        );
    }

    #[test]
    fn configured_plaintext_base_is_canonical_for_bridge() {
        let coordinator = LanguageCoordinator::new();
        coordinator.load_settings(&crate::config::WorkspaceSettings {
            languages: std::collections::HashMap::from([(
                "plaintext".to_string(),
                crate::config::settings::LanguageSettings {
                    base: Some("python".to_string()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        });

        assert_eq!(
            InjectionResolver::resolve_language(&coordinator, "plaintext", ""),
            "python"
        );
    }

    #[test]
    fn explicit_plaintext_ignores_first_line_heuristics() {
        let coordinator = LanguageCoordinator::new();

        assert_eq!(
            InjectionResolver::resolve_language(
                &coordinator,
                "plaintext",
                "#!/usr/bin/env python\nprint('hello')",
            ),
            "plaintext"
        );
    }

    /// Helper: parse `text` with tree-sitter Rust, match `string_content` nodes
    /// via injection query, and return the `CacheableInjectionRegion` for the first match.
    fn cacheable_from_first_injection(text: &str) -> CacheableInjectionRegion {
        let mut parser = create_rust_parser();
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let query_str = r#"
            ((string_literal
              (string_content) @injection.content)
             (#set! injection.language "test"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let regions =
            collect_all_injections(&root, text, Some(&query)).expect("should find injections");
        assert!(!regions.is_empty(), "expected at least one injection");

        CacheableInjectionRegion::from_region_info(&regions[0], "test-id", text)
    }

    #[test]
    fn test_detect_nested_injections() {
        use tree_sitter::Parser;

        // Simulate a markdown file with a code block
        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).expect("load rust grammar");

        let text = r#"let x = "markdown with ```lua code```";"#;
        let tree = parser.parse(text, None).expect("parse rust");
        let root = tree.root_node();

        // Create a mock injection query that simulates nested injections
        let query_str = r#"
        (string_literal
          (string_content) @injection.content
          (#set! injection.language "markdown"))
        "#;

        let query = Query::new(&language, query_str).expect("valid query");

        // Find a node within the string content
        let node_in_string = find_node_at_byte(&root, 20).expect("node at position");

        // Detect injection with content
        let result = detect_injection(&node_in_string, &root, text, Some(&query), "rust");

        assert!(result.is_some());
        let (hierarchy, _content_node, _pattern_index, _offset) = result.unwrap();

        // Should detect rust -> markdown hierarchy
        assert_eq!(hierarchy, vec!["rust", "markdown"]);
    }

    #[test]
    fn test_detect_injection_with_static_language() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let re = Regex::new(r"^\d+$").unwrap(); }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create a query that matches Regex::new with static language
        let query_str = r#"
            (call_expression
              function: (scoped_identifier
                path: (identifier) @_regex
                (#eq? @_regex "Regex")
                name: (identifier) @_new
                (#eq? @_new "new"))
              arguments: (arguments
                (raw_string_literal
                  (string_content) @injection.content))
              (#set! injection.language "regex"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Find a node inside the regex string
        let node = find_node_at_byte(&root, 35); // Position in regex string
        assert!(node.is_some());

        let result = detect_injection(&node.unwrap(), &root, text, Some(&query), "rust");
        assert_eq!(
            result.map(|(h, _, _, _)| h),
            Some(vec!["rust".to_string(), "regex".to_string()])
        );
    }

    #[test]
    fn test_detect_injection_with_no_injection() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { println!("hello"); }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Query that won't match
        let query_str = r#"
            (call_expression
              function: (identifier) @_fn
              (#eq? @_fn "nonexistent")
              (arguments) @injection.content
              (#set! injection.language "test"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let node = find_node_at_byte(&root, 20); // Position in string
        assert!(node.is_some());

        let result = detect_injection(&node.unwrap(), &root, text, Some(&query), "rust");
        assert_eq!(result.map(|(h, _, _, _)| h), None);
    }

    #[test]
    fn test_detect_injection_without_query() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let node = root.child(0).unwrap();
        let result = detect_injection(&node, &root, text, None, "rust");
        assert_eq!(result, None);
    }

    #[test]
    fn test_is_node_within() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let x = 42; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let outer = root.child(0).unwrap(); // function_item
        let inner = find_node_at_byte(&root, 20).unwrap(); // Some node inside

        assert!(is_node_within(&inner, &outer));
        assert!(!is_node_within(&outer, &inner));
    }

    #[test]
    fn test_recursive_injection_depth_limit() {
        // Test that we can handle multiple levels of injection
        // This is a simple test - real recursive injection happens in refactor.rs

        let mut parser = create_rust_parser();
        let text = r#"fn main() { let x = "nested"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create a query that would inject strings as another language
        let query_str = r#"
        ((string_literal
          (string_content) @injection.content)
         (#set! injection.language "nested_lang"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let node = find_node_at_byte(&root, 22).expect("node in string");
        let result = detect_injection(&node, &root, text, Some(&query), "rust");

        assert!(result.is_some());
        let (hierarchy, _, _, _) = result.unwrap();
        assert_eq!(hierarchy, vec!["rust", "nested_lang"]);

        // The actual deep recursion is tested through integration with refactor.rs
        // where handle_nested_injection recursively processes injections
    }

    #[test]
    fn resolve_all_combines_regions_marked_combined() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let open = "<div>"; let close = "</div>"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"
                ((string_literal
                   (string_content) @injection.content)
                 (#set! injection.language "html")
                 (#set! injection.combined))
            "#,
        )
        .expect("valid query");
        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("combined");

        let resolved =
            InjectionResolver::resolve_all(&coordinator, &tracker, &uri, &tree, text, &query, 0);

        assert_eq!(resolved.len(), 1, "combined captures form one document");
        assert!(resolved[0].virtual_content.contains("<div>"));
        assert!(resolved[0].virtual_content.contains("</div>"));
        assert!(!resolved[0].virtual_content.contains("let close"));
        assert!(!resolved[0].contiguous);

        let regions = collect_all_injections(&tree.root_node(), text, Some(&query)).unwrap();
        let second_id = InjectionResolver::calculate_region_id(&tracker, &uri, &regions[1], 0)
            .unwrap()
            .to_string();
        let via_second = InjectionResolver::resolve_by_region_id(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            &second_id,
            0,
        )
        .expect("a non-first combined capture resolves the shared document");
        assert_eq!(via_second.region.region_id, resolved[0].region.region_id);
        assert_eq!(via_second.virtual_content, resolved[0].virtual_content);
    }

    #[test]
    fn single_combined_capture_remains_contiguous() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let html = "<div></div>"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"
                ((string_literal
                   (string_content) @injection.content)
                 (#set! injection.language "html")
                 (#set! injection.combined))
            "#,
        )
        .expect("valid query");

        let resolved = InjectionResolver::resolve_all(
            &test_coordinator(),
            &NodeTracker::new(),
            &test_uri("single_combined"),
            &tree,
            text,
            &query,
            0,
        );

        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].contiguous);
        assert_eq!(resolved[0].virtual_content, "<div></div>");
    }

    #[test]
    fn combined_captures_strip_blockquote_prefixes_and_record_offsets() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = concat!(
            "> ```python\n",
            "> if True:\n",
            ">   print(1)\n",
            "> ```\n",
            "> gap\n",
            "> ```python\n",
            ">   print(2)\n",
            "> ```\n",
        );
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md_language,
            r#"
                ((fenced_code_block
                   (info_string (language) @injection.language)
                   (code_fence_content) @injection.content)
                 (#set! injection.combined))
            "#,
        )
        .expect("valid query");

        let resolved = InjectionResolver::resolve_all(
            &test_coordinator(),
            &NodeTracker::new(),
            &test_uri("combined_blockquote"),
            &tree,
            text,
            &query,
            0,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].virtual_content,
            "if True:\n  print(1)\n\n\n\n  print(2)\n"
        );
        assert_eq!(
            resolved[0].line_column_offsets,
            vec![2, 2, 0, 0, 0, 2, 0, 0]
        );
        assert!(!resolved[0].contiguous);
    }

    #[test]
    fn combined_content_snaps_stale_included_start_to_char_boundary() {
        let text = "éx\n";
        let (content, offsets) = build_combined_virtual_content(
            text,
            0..text.len(),
            std::slice::from_ref(&(1..usize::MAX)),
        );

        assert_eq!(content, "x\n");
        assert_eq!(offsets, vec![1, 0]);
    }

    #[test]
    fn combined_content_snaps_stale_span_start_before_slicing() {
        let text = "éx\n";

        let (content, offsets) = build_combined_virtual_content(
            text,
            1..text.len(),
            std::slice::from_ref(&(1..text.len())),
        );

        assert_eq!(content, "x\n");
        assert_eq!(offsets, vec![1, 0]);
    }

    #[test]
    fn combined_blank_included_line_records_its_prefix_offset() {
        let text = "> \n";
        let (content, offsets) = build_combined_virtual_content(
            text,
            0..text.len(),
            std::slice::from_ref(&(2..text.len())),
        );

        assert_eq!(content, "\n");
        assert_eq!(offsets, vec![2, 0]);
    }

    #[test]
    fn single_combined_blockquote_uses_contiguous_single_region_mapping() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "> ```python\n> if True:\n>   print(1)\n> ```\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md_language,
            r#"
                ((fenced_code_block
                   (info_string (language) @injection.language)
                   (code_fence_content) @injection.content)
                 (#set! injection.combined))
            "#,
        )
        .expect("valid query");

        let resolved = InjectionResolver::resolve_all(
            &test_coordinator(),
            &NodeTracker::new(),
            &test_uri("single_combined_blockquote"),
            &tree,
            text,
            &query,
            0,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].virtual_content, "if True:\n  print(1)\n");
        assert_eq!(resolved[0].line_column_offsets, vec![2, 2, 0]);
        assert!(resolved[0].contiguous);
    }

    #[test]
    fn combined_patterns_with_offsets_remain_separate() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "abc"; let b = "def"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"
                ((string_literal
                   (string_content) @injection.content)
                 (#set! injection.language "html")
                 (#set! injection.combined)
                 (#offset! @injection.content 0 1 0 -1))
            "#,
        )
        .expect("valid query");

        let resolved = InjectionResolver::resolve_all(
            &test_coordinator(),
            &NodeTracker::new(),
            &test_uri("combined_offset"),
            &tree,
            text,
            &query,
            0,
        );

        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].virtual_content, "b");
        assert_eq!(resolved[1].virtual_content, "e");
        assert!(resolved.iter().all(|region| region.contiguous));
    }

    #[test]
    fn trim_directive_adjusts_injection_content() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let value = "  body  "; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"
                ((string_content) @injection.content
                 (#set! injection.language "html")
                 (#trim! @injection.content 0 1 0 1))
            "#,
        )
        .expect("valid query");

        let resolved = InjectionResolver::resolve_all(
            &test_coordinator(),
            &NodeTracker::new(),
            &test_uri("trimmed_content"),
            &tree,
            text,
            &query,
            0,
        );

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].virtual_content, "body");
    }

    #[test]
    fn one_adjusted_capture_disables_combining_for_its_whole_group() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "body"; let b = "  body  "; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"((string_content) @injection.content
                 (#set! injection.language "html")
                 (#set! injection.combined)
                 (#trim! @injection.content 0 1 0 1))"#,
        )
        .unwrap();

        let regions = collect_all_injections(&tree.root_node(), text, Some(&query)).unwrap();

        assert_eq!(regions.len(), 2);
        assert!(regions.iter().all(|region| !region.combined));
    }

    #[test]
    fn same_range_injections_keep_distinct_languages() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { /* comment */ }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create a mock query that would inject the same node twice
        // This simulates what happens with luadoc -> comment
        let query_str = r#"
        ((block_comment) @injection.content
         (#set! injection.language "doc"))

        ((block_comment) @injection.content
         (#set! injection.language "comment"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Find a node inside the comment
        // The injection query matches on block_comment nodes, so we need to be inside one
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, root, text.as_bytes());

        let mut injection_count = 0;
        while let Some(_match) = matches.next() {
            injection_count += 1;
        }

        // This should find 2 matches (both patterns match the same comment)
        assert_eq!(injection_count, 2, "Expected 2 injection patterns to match");

        // Now test our detection from inside the comment
        let node_in_comment = find_node_at_byte(&root, 14).expect("node in comment");
        let result = detect_injection(&node_in_comment, &root, text, Some(&query), "rust");

        assert!(result.is_some(), "Should find injection");
        let (hierarchy, _, _, _) = result.unwrap();
        assert_eq!(
            hierarchy,
            vec!["rust", "doc"],
            "same-range alternatives use the first query pattern instead of fabricating nesting"
        );
    }

    #[test]
    fn same_range_same_language_keeps_distinct_pattern_semantics() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { /* comment */ }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"
            ((block_comment) @injection.content
             (#set! injection.language "comment"))
            ((block_comment) @injection.content
             (#set! injection.language "comment")
             (#set! injection.include-children))
            "#,
        )
        .expect("valid query");

        let regions = collect_all_injections(&tree.root_node(), text, Some(&query)).unwrap();

        assert_eq!(regions.len(), 2);
        assert_ne!(regions[0].include_children, regions[1].include_children);
        assert_ne!(regions[0].identity_slot, regions[1].identity_slot);
    }

    #[test]
    fn dynamic_languages_from_one_pattern_get_distinct_identity_slots() {
        let tracker = NodeTracker::new();
        let uri = test_uri("dynamic_identity");
        assert_ne!(
            tracker
                .named_layer_for_incarnation(&uri, 7, "javascript", 0)
                .unwrap(),
            tracker
                .named_layer_for_incarnation(&uri, 7, "typescript", 0)
                .unwrap(),
            "one dynamic-language pattern must not alias distinct virtual documents"
        );
    }

    #[test]
    fn same_range_bridge_resolution_uses_query_pattern_priority() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { /* comment */ }"#;
        let tree = parse_rust_code(&mut parser, text);
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"
                ((block_comment) @injection.content
                 (#set! injection.language "doc"))
                ((block_comment) @injection.content
                 (#set! injection.language "comment"))
            "#,
        )
        .expect("valid query");

        let all = collect_all_injections(&tree.root_node(), text, Some(&query)).unwrap();
        assert_eq!(
            all.len(),
            2,
            "both same-range languages remain discoverable"
        );
        let tracker = NodeTracker::new();
        let uri = test_uri("same_range_priority");
        let resolved_all = InjectionResolver::resolve_all(
            &test_coordinator(),
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            0,
        );
        assert_ne!(
            resolved_all[0].region.region_id, resolved_all[1].region.region_id,
            "alternate language layers at one host range need distinct identities"
        );
        let second_id = resolved_all[1].region.region_id.clone();
        let second = InjectionResolver::resolve_by_region_id(
            &test_coordinator(),
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            &second_id,
            0,
        )
        .expect("the exact alternate layer resolves from its region ID");
        assert_eq!(second.injection_language, "comment");
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &test_coordinator(),
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            15,
            0,
            RegionBoundary::HalfOpen,
        )
        .expect("comment position resolves");

        assert_eq!(resolved.region.language, "doc");
    }

    #[test]
    fn test_cacheable_injection_region_from_region_info() {
        // Create a parser and parse some code to get a real Node
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "hello"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Create an injection query that matches the string
        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "markdown"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Get injection regions
        let regions = collect_all_injections(&root, text, Some(&query));
        let regions = regions.expect("Should find injections");
        assert!(!regions.is_empty(), "Should find at least one injection");

        let region_info = &regions[0];

        // Convert to CacheableInjectionRegion (owned, no lifetime)
        let cacheable =
            CacheableInjectionRegion::from_region_info(region_info, "test-result-id", text);

        // Verify all fields are captured correctly
        assert_eq!(cacheable.language, "markdown");
        assert_eq!(
            cacheable.byte_range.start,
            region_info.content_node.start_byte()
        );
        assert_eq!(
            cacheable.byte_range.end,
            region_info.content_node.end_byte()
        );
        assert_eq!(
            cacheable.line_range.start,
            region_info.content_node.start_position().row as u32
        );
        assert_eq!(
            cacheable.line_range.end,
            region_info.content_node.start_position().row as u32 + 1
        );
        assert_eq!(cacheable.region_id, "test-result-id");
    }

    #[test]
    fn test_from_region_info_applies_column_offset() {
        // #offset! with a column delta (e.g. regex content after a prefix)
        // must shift byte_range and start_column to the effective position,
        // so the bridge extracts the right content and translates coordinates
        // correctly (#183).
        let mut parser = create_rust_parser();
        let text = "// regex content";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let query_str = r#"
            ((line_comment) @injection.content
              (#set! injection.language "regex")
              (#offset! @injection.content 0 3 0 0))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let regions = collect_all_injections(&root, text, Some(&query)).expect("injections");
        assert_eq!(regions.len(), 1);
        let cacheable = CacheableInjectionRegion::from_region_info(&regions[0], "test-id", text);

        assert_eq!(&text[cacheable.byte_range.clone()], "regex content");
        assert_eq!(cacheable.byte_range, 3..text.len());
        assert_eq!(cacheable.start_column, 3);
        assert_eq!(cacheable.line_range, 0..1);
    }

    #[test]
    fn test_from_region_info_applies_row_offset_for_frontmatter() {
        // The vendored markdown query trims `---` frontmatter delimiters via
        // (#offset! @injection.content 1 0 -1 0); the cacheable region must
        // reflect the effective (delimiter-free) range (#183).
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: x\n---\n\n# heading\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let root = tree.root_node();

        let query_str = r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#set! injection.include-children)
              (#offset! @injection.content 1 0 -1 0))
        "#;
        let query = Query::new(&md_language, query_str).expect("valid query");

        let regions = collect_all_injections(&root, text, Some(&query)).expect("injections");
        assert_eq!(regions.len(), 1);
        let cacheable = CacheableInjectionRegion::from_region_info(&regions[0], "test-id", text);

        assert_eq!(&text[cacheable.byte_range.clone()], "title: x\n");
        assert_eq!(cacheable.line_range, 1..2);
        assert_eq!(cacheable.start_column, 0);
    }

    #[test]
    fn test_resolved_injection_virtual_content_honors_offset() {
        // End-to-end through the bridge resolution path: the virtual document
        // sent downstream must contain only the effective (post-offset)
        // content, not the raw node text with delimiters (#183).
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: x\n---\n\n# heading\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query_str = r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#set! injection.include-children)
              (#offset! @injection.content 1 0 -1 0))
        "#;
        let query = Query::new(&md_language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("offset_frontmatter");

        let resolved =
            InjectionResolver::resolve_all(&coordinator, &tracker, &uri, &tree, text, &query, 0);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].virtual_content, "title: x\n");
        assert_eq!(resolved[0].region.line_range.start, 1);
        assert_eq!(resolved[0].line_column_offsets, vec![0]);
    }

    #[test]
    fn test_resolved_virtual_content_combines_offset_with_child_exclusion() {
        // #186: a query with #offset! but WITHOUT injection.include-children.
        // Blockquote `> ` prefixes (block_continuation children) must still be
        // stripped, restricted to the post-offset window — previously the
        // child-exclusion step was skipped entirely whenever an offset was
        // active, leaking prefixes into the virtual document.
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "> ```lua\n> local a = 1\n> local b = 2\n> ```\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query_str = r#"
            ((fenced_code_block
               (info_string (language) @injection.language)
               (code_fence_content) @injection.content)
              (#offset! @injection.content 1 0 0 0))
        "#;
        let query = Query::new(&md_language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("offset_blockquote");

        let resolved =
            InjectionResolver::resolve_all(&coordinator, &tracker, &uri, &tree, text, &query, 0);
        assert_eq!(resolved.len(), 1);
        // The row offset trims the first content line; child exclusion strips
        // the remaining `> ` prefixes.
        assert_eq!(resolved[0].virtual_content, "local b = 2\n");
    }

    #[test]
    fn test_resolve_injection_returns_ulid_format() {
        // Test that resolved injection has region_id in ULID format (26 chars)
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "hello"; }"#;
        let tree = parse_rust_code(&mut parser, text);

        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("ulid_format");

        // Resolve injection at byte offset inside the string literal
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            22,
            0,
            RegionBoundary::HalfOpen,
        );
        assert!(resolved.is_some(), "Should resolve injection");
        let region_id = resolved.unwrap().region.region_id;
        assert_eq!(
            region_id.len(),
            26,
            "ULID should be 26 characters, got: {}",
            region_id
        );
    }

    #[test]
    fn test_resolve_injection_multiple_regions_stable_ulids() {
        // Test that multiple injection regions get stable ULIDs for same ordinal
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "hello"; let b = "world"; let c = "test"; }"#;
        let tree = parse_rust_code(&mut parser, text);

        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("multiple");

        // Find byte offsets for each string
        let query_all = Query::new(&language, r#"(string_literal) @str"#).expect("valid query");
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches_iter = cursor.matches(&query_all, tree.root_node(), text.as_bytes());
        let mut byte_offsets = Vec::new();
        while let Some(m) = matches_iter.next() {
            byte_offsets.push(m.captures[0].node.start_byte() + 1);
        }
        assert_eq!(byte_offsets.len(), 3, "Should find 3 strings");

        // Resolve each injection
        let r1 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[0],
            0,
            RegionBoundary::HalfOpen,
        );
        let r2 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[1],
            0,
            RegionBoundary::HalfOpen,
        );
        let r3 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[2],
            0,
            RegionBoundary::HalfOpen,
        );

        // Each should have different ULIDs (different ordinals)
        let id1 = r1.unwrap().region.region_id;
        let id2 = r2.unwrap().region.region_id;
        let id3 = r3.unwrap().region.region_id;
        assert_ne!(id1, id2, "Different ordinals should have different ULIDs");
        assert_ne!(id2, id3, "Different ordinals should have different ULIDs");
        assert_ne!(id1, id3, "Different ordinals should have different ULIDs");
    }

    #[test]
    fn test_resolve_injection_same_position_returns_consistent_region_id() {
        // Test that resolving the same position returns consistent region_id
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "hello"; }"#;
        let tree = parse_rust_code(&mut parser, text);

        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("consistent");

        // Resolve the same position multiple times
        let byte_offset = 22;
        let r1 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offset,
            0,
            RegionBoundary::HalfOpen,
        );
        let r2 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offset,
            0,
            RegionBoundary::HalfOpen,
        );

        assert_eq!(
            r1.unwrap().region.region_id,
            r2.unwrap().region.region_id,
            "Same position should return same region_id"
        );
    }

    #[test]
    fn test_calculate_region_id_different_positions_different_ulids() {
        // Test that different injection positions produce different ULIDs
        // Phase 2: Uses position-based keys (start_byte, end_byte, kind)
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "lua1"; let b = "python"; let c = "lua2"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let mut cursor = tree_sitter::QueryCursor::new();
        let query_str = r#"(string_literal) @str"#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let mut matches_iter = cursor.matches(&query, root, text.as_bytes());
        let mut nodes = Vec::new();
        while let Some(m) = matches_iter.next() {
            nodes.push(m.captures[0].node);
        }
        assert_eq!(nodes.len(), 3, "Should find 3 strings");

        // Create injection regions manually: lua, python, lua
        let injections = [
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[0],
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: nodes[1],
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[2],
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
        ];

        let tracker = NodeTracker::new();
        let uri = test_uri("mixed");

        // Phase 2: calculate_region_id uses position-based keys (not ordinals)
        // Different positions → different ULIDs regardless of language
        let ulid_0 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[0], 0)
            .expect("unmanaged test tracker admits the mint");
        let ulid_1 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[1], 0)
            .expect("unmanaged test tracker admits the mint");
        let ulid_2 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[2], 0)
            .expect("unmanaged test tracker admits the mint");

        // All different because they have different byte positions
        assert_ne!(
            ulid_0, ulid_1,
            "Different positions should have different ULIDs"
        );
        assert_ne!(
            ulid_1, ulid_2,
            "Different positions should have different ULIDs"
        );
        assert_ne!(
            ulid_0, ulid_2,
            "Different positions should have different ULIDs"
        );

        // Same position returns same ULID (stability)
        let ulid_0_again =
            InjectionResolver::calculate_region_id(&tracker, &uri, &injections[0], 0)
                .expect("same-lifetime remint is admitted");
        assert_eq!(
            ulid_0, ulid_0_again,
            "Same position key should return same ULID"
        );
    }

    #[test]
    fn test_find_injection_at_position_returns_correct_region_and_index() {
        // Test that find_injection_at_position returns the correct region and its index
        // for use with calculate_region_id

        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "lua1"; let b = "py"; let c = "lua2"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Find all string_literal nodes
        let mut cursor = tree_sitter::QueryCursor::new();
        let query_str = r#"(string_literal) @str"#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let mut matches_iter = cursor.matches(&query, root, text.as_bytes());
        let mut nodes = Vec::new();
        while let Some(m) = matches_iter.next() {
            nodes.push(m.captures[0].node);
        }

        assert_eq!(nodes.len(), 3, "Should find 3 strings");

        // Create injection regions: lua, python, lua
        let injections = vec![
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[0],
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: nodes[1],
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[2],
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
        ];

        // Test finding position inside first Lua block
        let lua1_byte = nodes[0].start_byte() + 1; // Inside first string
        let result =
            find_injection_at_position(&injections, lua1_byte, text, RegionBoundary::HalfOpen);
        assert!(result.is_some(), "Should find injection at lua1 position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 0, "Should be at index 0");
        assert_eq!(region.language, "lua", "Should be lua region");

        // Test finding position inside Python block
        let py_byte = nodes[1].start_byte() + 1;
        let result =
            find_injection_at_position(&injections, py_byte, text, RegionBoundary::HalfOpen);
        assert!(result.is_some(), "Should find injection at python position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 1, "Should be at index 1");
        assert_eq!(region.language, "python", "Should be python region");

        // Test finding position inside second Lua block
        let lua2_byte = nodes[2].start_byte() + 1;
        let result =
            find_injection_at_position(&injections, lua2_byte, text, RegionBoundary::HalfOpen);
        assert!(result.is_some(), "Should find injection at lua2 position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 2, "Should be at index 2");
        assert_eq!(region.language, "lua", "Should be lua region");

        // Test position outside all injections
        let outside_byte = 5; // Position before any string
        let result =
            find_injection_at_position(&injections, outside_byte, text, RegionBoundary::HalfOpen);
        assert!(
            result.is_none(),
            "Should not find injection outside regions"
        );
    }

    /// The insert-mode caret at the end byte of a mid-line-ending region: the
    /// default half-open rule keeps it outside (node-reference-protocol), but
    /// `CaretEndFallback` — the mode caret-shaped methods like completion
    /// request — must resolve the region the user is visibly typing at the
    /// tail of.
    #[test]
    fn caret_end_fallback_resolves_mid_line_region_end() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "git co"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let query_str = r#"
            ((string_literal) @injection.content
              (#set! injection.language "lua"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");
        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("caret_end_mid_line");

        // The string_literal node ends right before the `;`, mid-line.
        let end = text.find(';').expect("statement end");
        assert!(
            InjectionResolver::resolve_at_byte_offset(
                &coordinator,
                &tracker,
                &uri,
                &tree,
                text,
                &query,
                end,
                0,
                RegionBoundary::HalfOpen,
            )
            .is_none(),
            "half-open must keep the end byte outside"
        );
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            end,
            0,
            RegionBoundary::CaretEndFallback,
        )
        .expect("caret fallback must resolve the mid-line region end");
        assert_eq!(resolved.region.language, "lua");
    }

    /// A region ending at column 0 (fenced-block shape): the caret at that
    /// byte sits on the closing fence line, the ADR's canonical "outside"
    /// example. The fallback must NOT fire there — every caret position on the
    /// block's last content line is already inside half-open.
    #[test]
    fn caret_end_fallback_keeps_fence_line_outside() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "```lua\nprint(1)\n```\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md_language,
            r#"
                ((fenced_code_block
                   (info_string (language) @injection.language)
                   (code_fence_content) @injection.content))
            "#,
        )
        .expect("valid query");
        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("caret_end_fence");

        // code_fence_content is "print(1)\n", ending at the start of the
        // closing-fence line (column 0).
        let fence_line_start = text.find("\n```").expect("closing fence") + 1;
        assert!(
            InjectionResolver::resolve_at_byte_offset(
                &coordinator,
                &tracker,
                &uri,
                &tree,
                text,
                &query,
                fence_line_start,
                0,
                RegionBoundary::CaretEndFallback,
            )
            .is_none(),
            "a column-0 region end is the closing fence — the fallback must not fire"
        );
        // Sanity: the last content byte still resolves under the fallback
        // mode's half-open primary scan — the fallback must not break
        // ordinary interior containment.
        assert!(
            InjectionResolver::resolve_at_byte_offset(
                &coordinator,
                &tracker,
                &uri,
                &tree,
                text,
                &query,
                fence_line_start - 1,
                0,
                RegionBoundary::CaretEndFallback,
            )
            .is_some(),
            "the newline before the fence is inside the region"
        );
    }

    /// The mid-line rule is judged at the **effective** end, not at the raw
    /// node's tree-sitter column. An `#offset!` that trims back across a
    /// newline leaves the region ending at column 0 — the fenced-block shape
    /// the fallback must decline — even though the raw node ends mid-line.
    ///
    /// This is what makes `ends_mid_line` load-bearing rather than a
    /// refactor: keeping the old `content_node.end_position().column` here
    /// would agree with it on every offset-free region, and only diverge when
    /// a directive moves the end across a `\n`.
    #[test]
    fn caret_fallback_judges_the_mid_line_rule_at_the_effective_end() {
        let mut parser = create_rust_parser();
        let text = "x\ny";
        let tree = parse_rust_code(&mut parser, text);
        let node = tree.root_node();
        assert_eq!(
            node.end_byte(),
            3,
            "fixture: the raw node spans the document"
        );
        assert!(
            node.end_position().column > 0,
            "fixture: the raw end is mid-line, so the old rule would fire"
        );

        let trim_across_newline = InjectionOffset {
            start_row: 0,
            start_column: 0,
            end_row: 0,
            end_column: -1,
        };
        let injections = vec![InjectionRegionInfo {
            language: "lua".to_string(),
            content_node: node,
            pattern_index: 0,
            include_children: false,
            offset: Some(trim_across_newline),
            combined: false,
            identity_slot: 0,
        }];
        assert_eq!(
            effective_content_range(&injections[0], text),
            0..2,
            "fixture: the offset trims the end back onto the newline"
        );

        assert!(
            find_injection_at_position(&injections, 2, text, RegionBoundary::CaretEndFallback)
                .is_none(),
            "the effective end sits at column 0, so the fallback must decline"
        );
    }

    /// A region ending with a trailing newline AT EOF (an unclosed block whose
    /// last typed character was Enter): the caret on the file's empty last
    /// line is still *inside* the unclosed injection — there is no closing
    /// fence line for the column-0 rule to protect. The end-of-document
    /// exception (mirroring the ADR's `b == L && e == L` rule) must let the
    /// fallback fire.
    #[test]
    fn caret_end_fallback_resolves_trailing_newline_region_at_eof() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        // Unclosed fence; the document ends right after the newline the user
        // just typed, so the content node ends at EOF, column 0.
        let text = "```lua\nprint(\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md_language,
            r#"
                ((fenced_code_block
                   (info_string (language) @injection.language)
                   (code_fence_content) @injection.content))
            "#,
        )
        .expect("valid query");
        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("caret_end_eof_newline");

        let resolved = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            text.len(),
            0,
            RegionBoundary::CaretEndFallback,
        )
        .expect("the caret on the empty last line of an unclosed block resolves");
        assert_eq!(resolved.region.language, "lua");
    }

    /// At an adjacency `end(A) == start(B)`, half-open containment (B) must
    /// keep winning under `CaretEndFallback` — the fallback only fires when no
    /// region contains the byte at all.
    #[test]
    fn caret_end_fallback_prefers_containing_region_at_adjacency() {
        let mut parser = create_rust_parser();
        let text = "fn f(){}";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();
        // The `(` and `)` tokens touch: [4,5) and [5,6).
        let a = root.descendant_for_byte_range(4, 5).expect("( token");
        let b = root.descendant_for_byte_range(5, 6).expect(") token");
        assert_eq!(a.end_byte(), b.start_byte(), "fixture regions must touch");

        let injections = vec![
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: a,
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: b,
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
        ];

        // byte 5 == end(A) == start(B): containment in B wins, not A's end.
        let (_, region) =
            find_injection_at_position(&injections, 5, text, RegionBoundary::CaretEndFallback)
                .expect("B contains byte 5");
        assert_eq!(region.language, "python");

        // byte 6 == end(B), mid-line, contained nowhere: the fallback fires.
        let (_, region) =
            find_injection_at_position(&injections, 6, text, RegionBoundary::CaretEndFallback)
                .expect("caret fallback at the trailing edge of B");
        assert_eq!(region.language, "python");
    }

    /// Offset-FREE nested regions sharing an end byte: the fallback scans in
    /// `collect_all_injections`'s order (raw start ascending), so the first
    /// match wins — which is the outermost region exactly because no directive
    /// shifts either span here.
    ///
    /// Deliberately scoped: with an `#offset!` in play, raw order no longer
    /// tracks effective nesting, so "first match" and "outermost" can part
    /// company. See the ordering note on `find_injection_at_position`.
    #[test]
    fn caret_end_fallback_prefers_first_in_raw_order_at_shared_end_without_offsets() {
        let mut parser = create_rust_parser();
        let text = "fn f(){}";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();
        // `parameters` "()" spans [4,6); its `)` child spans [5,6) — same end.
        let outer = root.descendant_for_byte_range(4, 6).expect("parameters");
        let inner = root.descendant_for_byte_range(5, 6).expect(") token");
        assert_eq!(
            outer.end_byte(),
            inner.end_byte(),
            "fixture must share the end"
        );
        assert!(
            outer.start_byte() < inner.start_byte(),
            "outer must start first"
        );

        // collect_all_injections sorts by (start, end, …) ascending — outer first.
        let injections = vec![
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: outer,
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: inner,
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
        ];

        let (_, region) =
            find_injection_at_position(&injections, 6, text, RegionBoundary::CaretEndFallback)
                .expect("shared trailing edge resolves");
        assert_eq!(
            region.language, "lua",
            "the first region in raw order — here the outermost — must win"
        );
    }

    /// The other half of the fix, and the one with no downstream backstop: a
    /// trimmed region used to SHADOW its neighbours. Raw containment matched
    /// the outer region, the request then died at the bridge's bounds
    /// precheck, and the adjacent region never got a chance. Now the trimmed
    /// region steps aside and the neighbour wins the byte outright.
    #[test]
    fn a_trimmed_end_lets_the_adjacent_region_win_the_byte() {
        let mut parser = create_rust_parser();
        let text = "fn f(){}";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();
        // `parameters` "()" spans [4,6); its `)` child spans [5,6).
        let trimmed = root.descendant_for_byte_range(4, 6).expect("parameters");
        let neighbour = root.descendant_for_byte_range(5, 6).expect(") token");

        // `0 0 0 -1` pulls the outer region's end back to 5, so byte 5 is the
        // neighbour's alone rather than contained by both.
        let injections = vec![
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: trimmed,
                pattern_index: 0,
                include_children: false,
                offset: Some(InjectionOffset {
                    start_row: 0,
                    start_column: 0,
                    end_row: 0,
                    end_column: -1,
                }),
                combined: false,
                identity_slot: 0,
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: neighbour,
                pattern_index: 0,
                include_children: false,
                offset: None,
                combined: false,
                identity_slot: 0,
            },
        ];
        assert_eq!(
            effective_content_range(&injections[0], text),
            4..5,
            "fixture: the directive trims the outer region off byte 5"
        );

        for boundary in [RegionBoundary::HalfOpen, RegionBoundary::CaretEndFallback] {
            let (index, region) = find_injection_at_position(&injections, 5, text, boundary)
                .expect("byte 5 resolves to the adjacent region");
            assert_eq!(
                (index, region.language.as_str()),
                (1, "python"),
                "the trimmed region must not shadow its neighbour under {boundary:?}"
            );
        }
    }

    /// A zero-width region is not a licence to ignore the column-0 rule. An
    /// EMPTY frontmatter collapses to the start of the closing fence line, so
    /// the one caret position it could offer sits on the fence itself — the
    /// canonical "outside" case. It declines, exactly as a non-collapsed
    /// region ending at column 0 does.
    ///
    /// This is a deliberate behavior change from `origin/main`, which routed
    /// the byte through raw containment. It is the same change as the fence
    /// exclusion everywhere else, not a special case for zero width.
    #[test]
    fn a_collapse_onto_a_line_start_is_the_closing_fence_and_stays_outside() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\n---\nrest\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md_language,
            r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#offset! @injection.content 1 0 -1 0))
            "#,
        )
        .expect("valid frontmatter injection query");

        let injections = collect_all_injections(&tree.root_node(), text, Some(&query))
            .expect("empty frontmatter is still an injection");
        assert_eq!(
            effective_content_range(&injections[0], text),
            4..4,
            "fixture: an empty frontmatter collapses onto the closing fence"
        );

        for boundary in [RegionBoundary::HalfOpen, RegionBoundary::CaretEndFallback] {
            assert!(
                find_injection_at_position(&injections, 4, text, boundary).is_none(),
                "a collapse at column 0 is the closing fence: outside under {boundary:?}"
            );
        }
    }

    /// The end-of-document exception is judged on the effective end too: a
    /// directive that trims the end back means the region no longer reaches
    /// EOF, so the caret past the last byte belongs to the host. The raw node
    /// still ends at `doc_len` and would have matched.
    #[test]
    fn a_trimmed_end_gives_up_the_end_of_document_exception() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: awesome\n---\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md_language,
            r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#offset! @injection.content 1 0 -1 0))
            "#,
        )
        .expect("valid frontmatter injection query");

        let injections = collect_all_injections(&tree.root_node(), text, Some(&query))
            .expect("frontmatter injection is found");
        assert_eq!(
            injections[0].content_node.end_byte(),
            text.len(),
            "fixture: the raw node runs to the document end"
        );

        assert!(
            find_injection_at_position(
                &injections,
                text.len(),
                text,
                RegionBoundary::CaretEndFallback
            )
            .is_none(),
            "the effective end is the closing fence, not EOF — the exception must not fire"
        );
    }

    /// One `string_literal` region carrying the caller's `#offset!` — the shape
    /// the bundled rust `injections.scm` wraps an embedded regex in, where the
    /// directive is `0 1 0 -1` to trim the quotes. Reused here with trimming,
    /// extending, and collapsing directives, so the offset is a parameter.
    ///
    /// `include_children` matters whenever a test follows the region past the
    /// lookup: a rust `string_literal` has a named `string_content` child, so
    /// with `false` the extracted content is only the quotes and the gap math
    /// (not the offset) would dominate the outcome.
    ///
    /// Returns `(injections, raw_start, raw_end)`.
    fn string_literal_injection_with_offset<'t>(
        tree: &'t Tree,
        text: &str,
        offset: InjectionOffset,
        include_children: bool,
    ) -> (Vec<InjectionRegionInfo<'t>>, usize, usize) {
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, "(string_literal) @str").expect("valid query");
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
        let node = matches
            .next()
            .expect("fixture has a string literal")
            .captures[0]
            .node;
        (
            vec![InjectionRegionInfo {
                language: "regex".to_string(),
                content_node: node,
                pattern_index: 0,
                include_children,
                offset: Some(offset),
                combined: false,
                identity_slot: 0,
            }],
            node.start_byte(),
            node.end_byte(),
        )
    }

    /// `#offset!` trims the quotes, so the closing quote is host punctuation —
    /// not injected content. Half-open lookup (the point-shaped methods:
    /// hover, definition, …) must therefore keep it outside, even though it is
    /// inside the raw `content_node`.
    #[test]
    fn offset_trimmed_region_excludes_the_raw_tail_from_half_open() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "git co"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let trim_quotes = InjectionOffset {
            start_row: 0,
            start_column: 1,
            end_row: 0,
            end_column: -1,
        };
        let (injections, raw_start, raw_end) =
            string_literal_injection_with_offset(&tree, text, trim_quotes, false);

        assert!(
            find_injection_at_position(&injections, raw_end - 1, text, RegionBoundary::HalfOpen)
                .is_none(),
            "the closing quote is outside the effective content"
        );
        assert!(
            find_injection_at_position(&injections, raw_start, text, RegionBoundary::HalfOpen)
                .is_none(),
            "the opening quote is outside the effective content"
        );
        assert!(
            find_injection_at_position(&injections, raw_start + 1, text, RegionBoundary::HalfOpen)
                .is_some(),
            "the first content byte is still inside"
        );
    }

    /// The caret fallback anchors on the *effective* end too: it fires at the
    /// trimmed end (the closing quote's byte) and no longer at the raw end.
    #[test]
    fn offset_trimmed_region_moves_the_caret_fallback_to_the_effective_end() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "git co"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let trim_quotes = InjectionOffset {
            start_row: 0,
            start_column: 1,
            end_row: 0,
            end_column: -1,
        };
        let (injections, _raw_start, raw_end) =
            string_literal_injection_with_offset(&tree, text, trim_quotes, false);

        // Asserted as a PAIR. `.is_some()` alone would pass under the old raw
        // ranges too — there via half-open containment, not via the fallback —
        // so it takes the half-open rejection beside it to pin the mechanism.
        assert!(
            find_injection_at_position(&injections, raw_end - 1, text, RegionBoundary::HalfOpen)
                .is_none(),
            "half-open rejects the trimmed end..."
        );
        let (_, region) = find_injection_at_position(
            &injections,
            raw_end - 1,
            text,
            RegionBoundary::CaretEndFallback,
        )
        .expect("...and the caret rule accepts the same byte, via the fallback");
        assert_eq!(region.language, "regex");

        assert!(
            find_injection_at_position(
                &injections,
                raw_end,
                text,
                RegionBoundary::CaretEndFallback,
            )
            .is_none(),
            "the raw end is a full character past the injected content"
        );
    }

    /// The inverse gap: an `#offset!` that *extends* the end leaves bytes past
    /// the raw node genuinely injected, and they must be reachable.
    #[test]
    fn offset_extended_region_reaches_bytes_past_the_raw_node() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "git co"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let extend_end = InjectionOffset {
            start_row: 0,
            start_column: 0,
            end_row: 0,
            end_column: 1,
        };
        let (injections, _raw_start, raw_end) =
            string_literal_injection_with_offset(&tree, text, extend_end, true);

        let (_, region) =
            find_injection_at_position(&injections, raw_end, text, RegionBoundary::HalfOpen)
                .expect("the byte the offset added is inside the effective content");
        assert_eq!(region.language, "regex");
    }

    /// An `#offset!` whose bounds meet collapses the effective span to zero
    /// width. Half-open containment is then vacuous by arithmetic — there is no
    /// character to hover — but the caret rule still routes at the collapse
    /// byte: that position IS the whole (empty) injection, which is the first
    /// keystroke inside an embedded block the user just opened. `origin/main`
    /// routed it through raw containment, and nothing downstream objects — an
    /// empty virtual document maps the caret to a valid `(0, 0)`.
    #[test]
    fn offset_collapsed_region_routes_only_the_caret_at_its_collapse_byte() {
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "git co"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        // The string_literal spans [20, 28). Equal adjusted bounds collapse at
        // byte 24 without relying on Neovim's invalid-range fallback.
        let crossing = InjectionOffset {
            start_row: 0,
            start_column: 4,
            end_row: 0,
            end_column: -4,
        };
        let (injections, raw_start, raw_end) =
            string_literal_injection_with_offset(&tree, text, crossing, false);
        let collapse_byte = raw_start + 4;

        for byte in raw_start..=raw_end {
            assert!(
                find_injection_at_position(&injections, byte, text, RegionBoundary::HalfOpen)
                    .is_none(),
                "half-open containment is vacuous for a zero-width span (byte {byte})"
            );
        }

        let (_, region) = find_injection_at_position(
            &injections,
            collapse_byte,
            text,
            RegionBoundary::CaretEndFallback,
        )
        .expect("the caret sitting exactly at the zero-width injection routes into it");
        assert_eq!(region.language, "regex");

        for byte in (raw_start..=raw_end).filter(|byte| *byte != collapse_byte) {
            assert!(
                find_injection_at_position(
                    &injections,
                    byte,
                    text,
                    RegionBoundary::CaretEndFallback
                )
                .is_none(),
                "only the collapse byte routes under the caret rule (byte {byte})"
            );
        }

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("collapsed_region");
        let resolved = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &Query::new(
                &tree_sitter_rust::LANGUAGE.into(),
                r#"((string_literal) @injection.content
                     (#set! injection.language "regex")
                     (#offset! @injection.content 0 4 0 -4))"#,
            )
            .expect("valid collapsing injection query"),
            collapse_byte,
            0,
            RegionBoundary::CaretEndFallback,
        )
        .expect("a zero-width injection still resolves, to an empty virtual document");
        assert_eq!(
            resolved.virtual_content, "",
            "the virtual document of a collapsed region is empty, not the raw span"
        );
    }

    /// The motivating real-world shape (#996 item 1): markdown YAML frontmatter
    /// with `#offset! @injection.content 1 0 -1 0`. The closing `---` sits
    /// inside the raw `minus_metadata` node but outside the injected YAML, so
    /// no boundary rule may route a request there — while the trimmed body
    /// still resolves. Unlike the synthetic cases above this goes through
    /// `collect_all_injections`, so it also pins that the row-shaped directive
    /// is actually plumbed onto the region.
    #[test]
    fn frontmatter_closing_fence_is_outside_the_yaml_region() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: awesome\n---\n\n# heading\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query = Query::new(
            &md_language,
            r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#offset! @injection.content 1 0 -1 0))
            "#,
        )
        .expect("valid frontmatter injection query");

        let injections = collect_all_injections(&tree.root_node(), text, Some(&query))
            .expect("frontmatter injection is found");
        assert_eq!(injections.len(), 1);
        assert!(
            injections[0].offset.is_some(),
            "the row offset must reach the region"
        );

        let body_start = text.find("title").expect("fixture has a body line");
        let fence_start = text.rfind("---\n").expect("fixture has a closing fence");

        let (_, region) =
            find_injection_at_position(&injections, body_start, text, RegionBoundary::HalfOpen)
                .expect("the frontmatter body resolves");
        assert_eq!(region.language, "yaml");

        for boundary in [RegionBoundary::HalfOpen, RegionBoundary::CaretEndFallback] {
            assert!(
                find_injection_at_position(&injections, fence_start, text, boundary).is_none(),
                "the closing fence must stay outside the YAML region under {boundary:?}"
            );
        }
        // Pins the effective end from BELOW as well. Without this the test
        // catches an end one byte too late (byte 19 would become contained)
        // but not one byte too early — at 18 the fence is still uncontained
        // and `range.end != 19` keeps the fallback quiet, so every assertion
        // above would still pass.
        assert!(
            find_injection_at_position(
                &injections,
                fence_start - 1,
                text,
                RegionBoundary::HalfOpen
            )
            .is_some(),
            "the body's trailing newline is the last injected byte"
        );
    }

    /// The same frontmatter case through the bridge's actual entry point, so
    /// the fix is pinned at the API the LSP handlers call — not only at the
    /// private lookup. The virtual document a body position resolves to must
    /// also be the trimmed content, with no `---` in it.
    #[test]
    fn resolve_at_byte_offset_declines_the_frontmatter_closing_fence() {
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md_language).expect("set md language");
        let text = "---\ntitle: awesome\n---\n\n# heading\n";
        let tree = parser.parse(text, None).expect("parse markdown");

        let query = Query::new(
            &md_language,
            r#"
            ((minus_metadata) @injection.content
              (#set! injection.language "yaml")
              (#offset! @injection.content 1 0 -1 0))
            "#,
        )
        .expect("valid frontmatter injection query");

        let coordinator = test_coordinator();
        let tracker = NodeTracker::new();
        let uri = test_uri("frontmatter_fence");
        let fence_start = text.rfind("---\n").expect("fixture has a closing fence");
        let body_start = text.find("title").expect("fixture has a body line");

        for boundary in [RegionBoundary::HalfOpen, RegionBoundary::CaretEndFallback] {
            assert!(
                InjectionResolver::resolve_at_byte_offset(
                    &coordinator,
                    &tracker,
                    &uri,
                    &tree,
                    text,
                    &query,
                    fence_start,
                    0,
                    boundary,
                )
                .is_none(),
                "no request may be routed into YAML from the closing fence under {boundary:?}"
            );
        }

        let resolved = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            body_start,
            0,
            RegionBoundary::HalfOpen,
        )
        .expect("the frontmatter body still resolves");
        assert_eq!(resolved.region.language, "yaml");
        assert_eq!(
            resolved.virtual_content, "title: awesome\n",
            "the virtual document is the trimmed content, fences excluded"
        );
    }

    #[test]
    fn test_collect_all_injections_respects_lua_match_predicate() {
        // Regression test: #lua-match? is a general predicate (not built-in to tree-sitter).
        // collect_all_injections must apply predicate filtering so that injection rules
        // guarded by #lua-match? only match when the predicate actually passes.
        //
        // Without filtering, a rule like:
        //   (string content: _ @injection.content (#lua-match? @injection.content "^;") (#set! injection.language "query"))
        // would match ALL strings, not just those starting with ";".
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let a = "hello"; let b = "; query"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        // Injection query with #lua-match? predicate:
        // Only strings starting with ";" should be injected as "query"
        let query_str = r#"
            ((string_literal
                (string_content) @injection.content)
              (#lua-match? @injection.content "^;")
              (#set! injection.language "query"))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let injections =
            collect_all_injections(&root, text, Some(&query)).expect("Should return Some");

        // Only "; query" should match, not "hello"
        assert_eq!(
            injections.len(),
            1,
            "Only strings matching #lua-match? should be injected, got: {:?}",
            injections
                .iter()
                .map(|i| &text[i.content_node.start_byte()..i.content_node.end_byte()])
                .collect::<Vec<_>>()
        );
        let content =
            &text[injections[0].content_node.start_byte()..injections[0].content_node.end_byte()];
        assert!(
            content.starts_with(';'),
            "Injected content should start with ';', got: {:?}",
            content
        );
    }

    #[test]
    fn test_collect_all_injections_respects_predicate_on_helper_capture() {
        let mut parser = create_rust_parser();
        let text = "foo!(x)";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();

        let query_str = r#"
            ((macro_invocation
                macro: (identifier) @_macro
                (token_tree) @injection.content)
              (#lua-match? @_macro "^html$")
              (#set! injection.language "html"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let injections =
            collect_all_injections(&root, text, Some(&query)).expect("injection collection");

        assert!(
            injections.is_empty(),
            "a failed helper-capture predicate must reject the whole match"
        );
    }

    #[test]
    fn test_detect_injection_respects_predicate_on_helper_capture() {
        let mut parser = create_rust_parser();
        let text = "foo!(x)";
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();
        let node = find_node_at_byte(&root, 5).expect("node inside macro token tree");

        let query_str = r#"
            ((macro_invocation
                macro: (identifier) @_macro
                (token_tree) @injection.content)
              (#lua-match? @_macro "^html$")
              (#set! injection.language "html"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        let injection = detect_injection(&node, &root, text, Some(&query), "rust");

        assert!(
            injection.is_none(),
            "a failed helper-capture predicate must reject point detection"
        );
    }

    #[rstest]
    #[case::single_line_no_trailing_newline(
        // "hello" sits entirely on row 0; no trailing newline → exclusive end = 1
        r#"let s = "hello";"#,
        0..1,
    )]
    #[case::multi_line_trailing_newline(
        // string_content starts at the byte after `"` on row 0; the content
        // ends with `\n` so end_position().column == 0 at row 4 → exclusive end = 4.
        "let s = \"\nline1\nline2\nline3\n\";",
        0..4,
    )]
    #[case::multi_line_no_trailing_newline(
        // string_content starts on row 0; last line has content (no trailing \n),
        // so end_position().column > 0 at row 2 → exclusive end = 3.
        "let s = \"\nline1\nline2\";",
        0..3,
    )]
    #[trace]
    fn test_line_range_edge_cases(
        #[case] text: &str,
        #[case] expected_line_range: std::ops::Range<u32>,
    ) {
        let cacheable = cacheable_from_first_injection(text);
        assert_eq!(
            cacheable.line_range, expected_line_range,
            "line_range mismatch for text: {:?}",
            text
        );
    }

    #[test]
    fn collects_injected_languages_from_markdown_code_blocks() {
        use std::collections::HashSet;
        use tree_sitter::Query;

        let markdown_text = r#"# Example

```lua
print("Hello from Lua")
local x = 42
```

Some text.

```python
def hello():
    print("Hello from Python")
```

```lua
local y = "duplicate"
```
"#;

        let mut parser = Parser::new();
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser.set_language(&md_language).expect("set markdown");
        let tree = parser.parse(markdown_text, None).expect("parse markdown");
        let root = tree.root_node();

        let injection_query_str = r#"
            (fenced_code_block
              (info_string
                (language) @injection.language)
              (code_fence_content) @injection.content)
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("valid injection query");

        let injections = collect_all_injections(&root, markdown_text, Some(&injection_query))
            .unwrap_or_default();

        let unique_languages: HashSet<String> =
            injections.iter().map(|i| i.language.clone()).collect();

        assert_eq!(unique_languages.len(), 2);
        assert!(unique_languages.contains("lua"));
        assert!(unique_languages.contains("python"));
        assert_eq!(injections.len(), 3, "2 lua + 1 python");
    }

    #[test]
    fn injection_discovery_stops_during_cancelled_walk() {
        let text = (0..80)
            .map(|i| format!("```lua\nprint({i})\n```\n"))
            .collect::<String>();
        let language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(&text, None).unwrap();
        let query = Query::new(
            &language,
            r#"(fenced_code_block
                  (info_string (language) @injection.language)
                  (code_fence_content) @injection.content)"#,
        )
        .unwrap();
        let cancel = crate::cancel::CancelToken::default();
        // Entry consumes the first poll; the next periodic walk checkpoint
        // flips the token after discovery has started.
        cancel.cancel_after_polls(2);

        let regions = collect_all_injections_cancellable(
            &tree.root_node(),
            &text,
            Some(&query),
            Some(&cancel),
        );

        assert!(regions.is_none(), "cancelled discovery must be discarded");
        assert!(
            cancel.is_cancelled(),
            "cancellation must occur during the walk"
        );
    }

    #[test]
    fn injection_resolution_stops_during_cancelled_walk() {
        let text = (0..80)
            .map(|i| format!("```lua\nprint({i})\n```\n"))
            .collect::<String>();
        let language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(&text, None).unwrap();
        let query = Query::new(
            &language,
            r#"(fenced_code_block
                  (info_string (language) @injection.language)
                  (code_fence_content) @injection.content)"#,
        )
        .unwrap();
        let regions = collect_all_injections(&tree.root_node(), &text, Some(&query)).unwrap();
        let cacheable = regions
            .iter()
            .enumerate()
            .map(|(i, region)| {
                CacheableInjectionRegion::from_region_info(region, &format!("region-{i}"), &text)
            })
            .collect::<Vec<_>>();
        let cancel = crate::cancel::CancelToken::default();
        cancel.cancel_after_polls(2);

        let resolved = InjectionResolver::resolve_from_prebuilt_cancellable(
            &LanguageCoordinator::new(),
            &regions,
            &cacheable,
            &text,
            Some(&cancel),
        );

        assert!(resolved.is_none());
        assert!(cancel.is_cancelled());
    }

    // --- stale-tree hardening (#401): byte offsets that no longer match `text`
    // must degrade gracefully instead of panicking. ---

    #[test]
    fn position_of_byte_out_of_bounds_does_not_panic() {
        // Offsets past the end, and an anchor landing mid-codepoint (byte 1 of あ).
        let _ = position_of_byte("あ", 99, 50, 0);
        let (row, col) = position_of_byte("あ", 1, 0, 0);
        assert_eq!((row, col), (0, 0));
    }

    #[test]
    fn from_region_info_stale_node_does_not_panic() {
        // A node parsed from `text` but resolved against a much shorter text
        // (as if the document shrank before the tree was refreshed).
        let mut parser = create_rust_parser();
        let text = r#"fn main() { let s = "a fairly long string literal"; }"#;
        let tree = parse_rust_code(&mut parser, text);
        let root = tree.root_node();
        let query_str = r#"((string_literal) @injection.content (#set! injection.language "md"))"#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");
        let regions = collect_all_injections(&root, text, Some(&query)).expect("injections");
        assert!(!regions.is_empty());

        let short_text = "x";
        let cacheable = CacheableInjectionRegion::from_region_info(&regions[0], "id", short_text);
        // Completed without panicking; metadata is still populated.
        assert_eq!(cacheable.language, "md");
        assert_eq!(cacheable.region_id, "id");
        // byte_range is snapped in-bounds, so a downstream `&text[byte_range]`
        // can't panic either.
        assert!(cacheable.byte_range.start <= short_text.len());
        assert!(cacheable.byte_range.end <= short_text.len());
        assert!(cacheable.byte_range.start <= cacheable.byte_range.end);
        assert_eq!(&short_text[cacheable.byte_range.clone()], "");
    }
}
