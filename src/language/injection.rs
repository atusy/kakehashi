use crate::language::LanguageCoordinator;
use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};
use crate::language::query_predicates::check_predicate;
use crate::language::region_id_tracker::RegionIdTracker;
use crate::text::fnv1a_hash;
use tree_sitter::{Node, Query, QueryCapture, QueryCursor, QueryMatch, StreamingIterator, Tree};
use ulid::Ulid;
use url::Url;

/// Represents offset adjustments for injection content boundaries
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InjectionOffset {
    pub start_row: i32,
    pub start_column: i32,
    pub end_row: i32,
    pub end_column: i32,
}

impl InjectionOffset {
    /// Create a new InjectionOffset
    pub fn new(start_row: i32, start_column: i32, end_row: i32, end_column: i32) -> Self {
        Self {
            start_row,
            start_column,
            end_row,
            end_column,
        }
    }
}

/// Default offset with no adjustments
pub const DEFAULT_OFFSET: InjectionOffset = InjectionOffset {
    start_row: 0,
    start_column: 0,
    end_row: 0,
    end_column: 0,
};

/// Parses offset directive for a specific pattern in the query.
/// Returns None if the specified pattern has no #offset! directive for @injection.content.
pub fn parse_offset_directive_for_pattern(
    query: &Query,
    pattern_index: usize,
) -> Option<InjectionOffset> {
    for predicate in get_all_predicates(query, pattern_index) {
        // Skip non-offset! directives
        if predicate.operator() != "offset!" {
            continue;
        }

        // Skip non-General predicates
        let UnifiedPredicate::General(pred) = predicate else {
            continue;
        };

        // Skip if first arg is not a capture
        let Some(tree_sitter::QueryPredicateArg::Capture(capture_id)) = pred.args.first() else {
            continue;
        };

        // Skip if capture name not found or not @injection.content
        let Some(_) = query
            .capture_names()
            .get(*capture_id as usize)
            .filter(|name| **name == "injection.content")
        else {
            continue;
        };

        // Parse the 4 numeric arguments after the capture
        // Format: (#offset! @injection.content start_row start_col end_row end_col)
        let arg_count = pred.args.len();

        // Validate argument count (should be 5: capture + 4 offsets)
        if arg_count < 5 {
            log::info!(
                target: "kakehashi::query",
                "Malformed #offset! directive for pattern {}: expected 4 offset values, got {}. \
                Using default offset (0, 0, 0, 0). \
                Correct format: (#offset! @injection.content start_row start_col end_row end_col)",
                pattern_index,
                arg_count - 1 // Subtract 1 for the capture argument
            );
            return Some(DEFAULT_OFFSET);
        }

        // Try to parse each argument as i32
        let parse_arg = |idx: usize| -> Result<i32, String> {
            if let Some(tree_sitter::QueryPredicateArg::String(s)) = pred.args.get(idx) {
                s.parse().map_err(|_| s.to_string())
            } else {
                Err(String::from("missing"))
            }
        };

        // Parse all 4 offset values
        let parse_results = [
            ("start_row", parse_arg(1)),
            ("start_col", parse_arg(2)),
            ("end_row", parse_arg(3)),
            ("end_col", parse_arg(4)),
        ];

        // Pattern match on all 4 results - more idiomatic than all(is_ok) + unwrap()
        if let [
            ("start_row", Ok(start_row)),
            ("start_col", Ok(start_col)),
            ("end_row", Ok(end_row)),
            ("end_col", Ok(end_col)),
        ] = parse_results.as_slice()
        {
            return Some(InjectionOffset::new(
                *start_row, *start_col, *end_row, *end_col,
            ));
        }

        // Log which values failed to parse
        let error_details: Vec<String> = parse_results
            .into_iter()
            .filter_map(|(name, result)| result.err().map(|val| format!("{} = '{}'", name, val)))
            .collect();

        log::info!(
            target: "kakehashi::query",
            "Failed to parse #offset! directive for pattern {}: invalid values [{}]. \
            Using default offset (0, 0, 0, 0). \
            All offset values must be integers.",
            pattern_index,
            error_details.join(", ")
        );

        return Some(DEFAULT_OFFSET);
    }
    None
}

/// Iterates over `@injection.content` captures that pass general predicate filtering.
///
/// Combines predicate evaluation (e.g., `#lua-match?`) and capture name checking
/// into a single iterator, eliminating duplication between `collect_all_injections`
/// and `extract_content_and_language`.
fn iter_valid_injection_content_captures<'a, 'b>(
    match_: &'b QueryMatch<'_, 'a>,
    query: &'b Query,
    text: &'b str,
) -> impl Iterator<Item = QueryCapture<'a>> + 'b {
    match_.captures.iter().copied().filter(move |capture| {
        if !check_predicate(query, match_, capture, text) {
            return false;
        }
        query
            .capture_names()
            .get(capture.index as usize)
            .is_some_and(|name| *name == "injection.content")
    })
}

/// Checks if a node is within the bounds of another node
fn is_node_within(node: &Node, container: &Node) -> bool {
    node.start_byte() >= container.start_byte() && node.end_byte() <= container.end_byte()
}

/// Extracts the injection language from query properties or captures
///
/// Handles three patterns:
/// 1. Static: `#set! injection.language "language_name"`
/// 2. Dynamic capture: `(language) @injection.language`
/// 3. nvim-treesitter custom: `#set-lang-from-info-string! @capture` (uses capture text as language)
fn extract_injection_language(query: &Query, match_: &QueryMatch, text: &str) -> Option<String> {
    // First check for static language via #set! property
    if let Some(language) = extract_static_language(query, match_) {
        return Some(language);
    }

    // Then check for nvim-treesitter's #set-lang-from-info-string! predicate
    if let Some(language) = extract_language_from_info_string(query, match_, text) {
        return Some(language);
    }

    // Finally check for dynamic language via @injection.language capture
    extract_dynamic_language(query, match_, text)
}

/// Checks whether the given pattern has `#set! injection.include-children`.
///
/// This is a value-less property — its **presence** alone means "include children"
/// (the injection parser sees the full content node including named children).
/// When absent, named children should be excluded via `set_included_ranges()`.
pub(crate) fn has_include_children_for_pattern(query: &Query, pattern_index: usize) -> bool {
    for predicate in get_all_predicates(query, pattern_index) {
        if let UnifiedPredicate::Property(prop) = predicate
            && prop.key.as_ref() == "injection.include-children"
        {
            return true;
        }
    }
    false
}

/// Compute the included ranges for an injection content node.
///
/// When `include_children` is false and the content node has named children
/// (e.g., `block_continuation` nodes in blockquote code blocks), this function
/// computes the "gap" ranges between those children — the actual code content.
///
/// Returns `None` when:
/// - `include_children` is true (the injection parser should see everything)
/// - The content node has no named children (no gaps to compute)
/// - All content is covered by named children (no gaps exist)
///
/// The returned ranges are **relative** to the content node's start position:
/// byte offsets are subtracted by `content_node.start_byte()`, and row/column
/// Points are adjusted relative to `content_node.start_position()`.
pub(crate) fn compute_included_ranges(
    content_node: &tree_sitter::Node,
    include_children: bool,
) -> Option<Vec<tree_sitter::Range>> {
    if include_children || content_node.named_child_count() == 0 {
        return None;
    }

    // Check if all named children are zero-width (ghost nodes from
    // tree-sitter parsing with included_ranges). If so, treat as if
    // there are no children — the gap computation would produce a
    // trivial single range covering the entire node.
    let has_nonzero_children = {
        let mut cursor = content_node.walk();
        content_node
            .named_children(&mut cursor)
            .any(|c| c.start_byte() != c.end_byte())
    };
    if !has_nonzero_children {
        return None;
    }

    let node_start_byte = content_node.start_byte();
    let node_start_pos = content_node.start_position();
    let node_end_byte = content_node.end_byte();
    let node_end_pos = content_node.end_position();

    // Helper to make a Point relative to the content node's start.
    // Column is only adjusted when the point is on the same row as the node start,
    // because only that row has a non-zero column offset from the node origin.
    let relativize_point = |p: tree_sitter::Point| -> tree_sitter::Point {
        tree_sitter::Point {
            row: p.row.saturating_sub(node_start_pos.row),
            column: if p.row == node_start_pos.row {
                p.column.saturating_sub(node_start_pos.column)
            } else {
                p.column
            },
        }
    };

    let mut ranges = Vec::new();
    let mut cursor_byte = node_start_byte;
    let mut cursor_point = node_start_pos;

    let mut tree_cursor = content_node.walk();
    for child in content_node.named_children(&mut tree_cursor) {
        let child_start_byte = child.start_byte();
        let child_end_byte = child.end_byte();

        // Skip zero-width children. Tree-sitter-markdown creates zero-width
        // block_continuation nodes when parsing with included_ranges that
        // already excluded the `> ` prefix bytes. These ghost nodes don't
        // span any bytes, so they don't define meaningful gaps.
        if child_start_byte == child_end_byte {
            continue;
        }

        // Gap before this child
        if cursor_byte < child_start_byte {
            ranges.push(tree_sitter::Range {
                start_byte: cursor_byte - node_start_byte,
                end_byte: child_start_byte - node_start_byte,
                start_point: relativize_point(cursor_point),
                end_point: relativize_point(child.start_position()),
            });
        }

        cursor_byte = child_end_byte;
        cursor_point = child.end_position();
    }

    // Gap after last child
    if cursor_byte < node_end_byte {
        ranges.push(tree_sitter::Range {
            start_byte: cursor_byte - node_start_byte,
            end_byte: node_end_byte - node_start_byte,
            start_point: relativize_point(cursor_point),
            end_point: relativize_point(node_end_pos),
        });
    }

    if ranges.is_empty() {
        None
    } else {
        Some(ranges)
    }
}

/// Sub-select parent `included_ranges` for a nested injection byte region.
///
/// When a nested injection has no `block_continuation` children of its own
/// (because its tree was parsed with `included_ranges`), we inherit the parent's
/// ranges by clipping them to the nested content region and re-relativizing.
///
/// Both `nested_start_byte` and `nested_end_byte` are relative to the same
/// base as `parent_ranges` (the parent's `content_text` start).
///
/// Returns `Some(clipped_ranges)` if any parent ranges overlap the nested region,
/// `None` otherwise.
pub(crate) fn sub_select_included_ranges(
    parent_ranges: &[tree_sitter::Range],
    nested_start_byte: usize,
    nested_end_byte: usize,
) -> Option<Vec<tree_sitter::Range>> {
    // Find the base row from the first overlapping range
    let first_overlap = parent_ranges
        .iter()
        .find(|r| r.end_byte > nested_start_byte && r.start_byte < nested_end_byte)?;
    let base_row = first_overlap.start_point.row;

    let mut clipped = Vec::new();
    let mut is_first = true;

    for r in parent_ranges {
        // Skip non-overlapping ranges
        if r.end_byte <= nested_start_byte || r.start_byte >= nested_end_byte {
            continue;
        }

        // Clip to nested region
        let clip_start = r.start_byte.max(nested_start_byte);
        let clip_end = r.end_byte.min(nested_end_byte);

        // The first sub-selected range's column must be 0 because the prefix
        // bytes (e.g., "> ") preceding this range are NOT in the nested
        // content_text — they come before nested_start_byte. Without this,
        // the column would double-count with content_start_col in the token
        // collector. Subsequent ranges keep the parent's column because their
        // prefix bytes ARE within the nested content_text.
        let start_column = if is_first { 0 } else { r.start_point.column };

        clipped.push(tree_sitter::Range {
            start_byte: clip_start - nested_start_byte,
            end_byte: clip_end - nested_start_byte,
            start_point: tree_sitter::Point {
                row: r.start_point.row - base_row,
                column: start_column,
            },
            end_point: tree_sitter::Point {
                row: r.end_point.row - base_row,
                column: r.end_point.column,
            },
        });

        is_first = false;
    }

    if clipped.is_empty() {
        None
    } else {
        Some(clipped)
    }
}

/// Extract clean injection content, stripping child node bytes when included_ranges present.
///
/// When `included_ranges` is `Some`, only the gap bytes (actual code content) are
/// concatenated, effectively stripping blockquote prefixes like `> `. When `None`,
/// returns the full content slice as-is.
///
/// # Arguments
/// * `host_text` - The full host document text
/// * `byte_range` - The byte range of the injection content node
/// * `included_ranges` - Gap ranges from `compute_included_ranges()`, relative to node start
pub(crate) fn extract_clean_content(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    included_ranges: Option<&[tree_sitter::Range]>,
) -> String {
    let content = &host_text[byte_range.clone()];
    match included_ranges {
        None => content.to_string(),
        Some(ranges) => {
            let capacity: usize = ranges.iter().map(|r| r.end_byte - r.start_byte).sum();
            let mut result = String::with_capacity(capacity);
            for range in ranges {
                result.push_str(&content[range.start_byte..range.end_byte]);
            }
            result
        }
    }
}

/// Compute per-virtual-line column offsets from included ranges.
///
/// For each virtual line in the injection, computes the UTF-16 column offset
/// (the width of the blockquote prefix or other excluded content before the
/// gap on that line).
///
/// When `included_ranges` is `None`, returns `vec![start_column]` (the
/// non-blockquote fallback — only line 0 has a column offset).
///
/// # Arguments
/// * `host_text` - The full host document text
/// * `byte_range` - The byte range of the injection content node
/// * `start_column` - UTF-16 column offset for the first line
/// * `included_ranges` - Gap ranges from `compute_included_ranges()`, relative to node start
pub(crate) fn compute_line_column_offsets(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    start_column: u32,
    included_ranges: Option<&[tree_sitter::Range]>,
) -> Vec<u32> {
    match included_ranges {
        None => vec![start_column],
        Some(ranges) => {
            // Build a map from virtual line → column offset.
            // Each gap range starts at a known point with a column value.
            // The column value represents how many columns of prefix exist
            // before the actual code on that line.
            let mut offsets = Vec::new();
            for range in ranges {
                let gap_start_line = range.start_point.row;
                // How many virtual lines does this gap span?
                let gap_end_line = range.end_point.row;
                for line in gap_start_line..=gap_end_line {
                    offsets.resize(line + 1, 0);
                    if line == gap_start_line {
                        // The column offset is the gap's start column within the host line.
                        // For the first line (row 0) of the content node, use start_column
                        // (which accounts for the full prefix from the line start).
                        // For subsequent lines, the start_point.column from
                        // compute_included_ranges is already the raw byte column
                        // within the line — convert to UTF-16.
                        if line == 0 {
                            offsets[line] = start_column;
                        } else {
                            // Convert byte column to UTF-16 code units.
                            // The gap's start_byte is relative to content node start.
                            // The line starts at the byte after the previous newline.
                            let gap_abs_byte = byte_range.start + range.start_byte;
                            let line_start_byte = gap_abs_byte - range.start_point.column;
                            let prefix = &host_text[line_start_byte..gap_abs_byte];
                            offsets[line] = prefix.encode_utf16().count() as u32;
                        }
                    }
                }
            }
            if offsets.is_empty() {
                vec![start_column]
            } else {
                offsets
            }
        }
    }
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
    let included_ranges = compute_included_ranges(&region.content_node, region.include_children);
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

/// Parse text with optional included ranges, resetting parser state afterward.
///
/// This is the shared protocol for interacting with Tree-sitter's
/// `set_included_ranges` API. Both the semantic tokens and selection range
/// paths delegate here to avoid divergence.
///
/// # Behavior
///
/// 1. If `included_ranges` is `Some`, calls `parser.set_included_ranges()`
/// 2. On failure, logs a warning, resets ranges, and returns `None`
/// 3. Calls `parser.parse(text, None)`
/// 4. Always resets `parser.set_included_ranges(&[])` after parsing
///
/// The unconditional reset ensures parsers returned to pools or caches
/// never carry stale included-range state.
pub(crate) fn parse_with_ranges(
    parser: &mut tree_sitter::Parser,
    text: &str,
    included_ranges: Option<&[tree_sitter::Range]>,
    log_target: &str,
    lang_name: &str,
) -> Option<tree_sitter::Tree> {
    if let Some(ranges) = included_ranges
        && let Err(e) = parser.set_included_ranges(ranges)
    {
        log::warn!(
            target: log_target,
            "Failed to set included ranges for {}: {}. Skipping parse.",
            lang_name, e
        );
        let _ = parser.set_included_ranges(&[]);
        return None;
    }

    let tree = parser.parse(text, None);

    // Always reset included ranges after parsing to prevent stale state
    let _ = parser.set_included_ranges(&[]);

    tree
}

/// Extracts language from #set! injection.language property
fn extract_static_language(query: &Query, match_: &QueryMatch) -> Option<String> {
    // Use unified accessor to check property settings
    for predicate in get_all_predicates(query, match_.pattern_index) {
        if let UnifiedPredicate::Property(prop) = predicate
            && prop.key.as_ref() == "injection.language"
            && let Some(value) = &prop.value
        {
            return Some(value.as_ref().to_string());
        }
    }
    None
}

/// Extracts language from @injection.language capture
fn extract_dynamic_language(query: &Query, match_: &QueryMatch, text: &str) -> Option<String> {
    for capture in match_.captures {
        if let Some(capture_name) = query.capture_names().get(capture.index as usize)
            && *capture_name == "injection.language"
        {
            let lang_text = &text[capture.node.byte_range()];
            return Some(lang_text.to_string());
        }
    }
    None
}

/// Extracts language from nvim-treesitter's #set-lang-from-info-string! predicate
///
/// This is a custom nvim-treesitter predicate that uses the text of a capture
/// as the injection language. It's commonly used for markdown fenced code blocks:
///
/// ```scheme
/// (fenced_code_block
///   (info_string (language) @_lang)
///   (code_fence_content) @injection.content
///   (#set-lang-from-info-string! @_lang))
/// ```
fn extract_language_from_info_string(
    query: &Query,
    match_: &QueryMatch,
    text: &str,
) -> Option<String> {
    // Look for #set-lang-from-info-string! predicate
    for predicate in get_all_predicates(query, match_.pattern_index) {
        if predicate.operator() == "set-lang-from-info-string!"
            && let UnifiedPredicate::General(pred) = predicate
        {
            // The predicate takes a capture reference as argument
            if let Some(tree_sitter::QueryPredicateArg::Capture(capture_id)) = pred.args.first() {
                // Find the capture in the match
                for capture in match_.captures {
                    if capture.index == *capture_id {
                        // Extract the text from the captured node as the language
                        let lang_text = &text[capture.node.byte_range()];
                        // Normalize the language name (lowercase, trim)
                        let normalized = lang_text.trim().to_lowercase();
                        if !normalized.is_empty() {
                            return Some(normalized);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Represents an injection region found in the document
#[derive(Debug, Clone)]
pub struct InjectionRegionInfo<'a> {
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
}

use std::ops::Range;

/// Owned injection region for caching (no lifetime dependency on parse tree)
///
/// Unlike `InjectionRegionInfo<'a>`, this struct owns all its data and can be
/// stored in caches that outlive the parse tree. Created via `from_region_info()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheableInjectionRegion {
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
    /// Generated by RegionIdTracker as a position-based ULID.
    pub region_id: String,
    /// Hash of the injection content for cache invalidation.
    /// Used to detect when cached semantic tokens should be invalidated:
    /// if content_hash changes (or language changes), the cached tokens are stale.
    pub content_hash: u64,
}

impl CacheableInjectionRegion {
    /// Create from an InjectionRegionInfo, extracting position data from the node.
    ///
    /// # Panics (debug builds)
    /// Panics if the content node is zero-width (`start_byte >= end_byte`).
    /// Callers must filter zero-width nodes upstream (see `collect_all_injections`).
    pub fn from_region_info(info: &InjectionRegionInfo<'_>, region_id: &str, text: &str) -> Self {
        let node = &info.content_node;
        debug_assert!(
            node.start_byte() < node.end_byte(),
            "from_region_info called with zero-width node at byte {}",
            node.start_byte(),
        );
        let content = &text[node.start_byte()..node.end_byte()];

        // Convert tree-sitter byte column to UTF-16 code units for LSP compatibility.
        // Tree-sitter reports columns as byte offsets, but LSP positions use UTF-16.
        // For ASCII they're identical, but non-ASCII prefixes (CJK, emoji) differ.
        // tree-sitter's `column` is a byte offset from the line start, so
        // `start_byte() - column` correctly recovers the line-start byte even
        // when the line contains multi-byte UTF-8 characters before the node.
        let byte_column = node.start_position().column;
        let line_start_byte = node.start_byte() - byte_column;
        let line_prefix = &text[line_start_byte..node.start_byte()];
        let start_column = line_prefix.encode_utf16().count() as u32;

        let start_line = node.start_position().row as u32;
        let end_pos = node.end_position();
        // end_position() points past the last byte. When that position is mid-line
        // (column > 0), the node still occupies that row → exclusive end is row + 1.
        // When column == 0 (node ended with newline), the row is already one past
        // the last occupied line — use it directly.
        let end_line = if end_pos.column > 0 {
            end_pos.row as u32 + 1
        } else {
            end_pos.row as u32
        };

        Self {
            language: info.language.clone(),
            byte_range: node.start_byte()..node.end_byte(),
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

    /// Check if a byte offset falls within this injection region's byte range.
    ///
    /// Used for determining which injection regions overlap with an edit.
    pub fn contains_byte(&self, byte: usize) -> bool {
        self.byte_range.contains(&byte)
    }

    /// Extract the injection content from the host document text.
    ///
    /// Returns the substring of `host_text` corresponding to this region's byte range.
    /// This is the "virtual document" content that would be sent to a language server.
    pub fn extract_content<'a>(&self, host_text: &'a str) -> &'a str {
        &host_text[self.byte_range.clone()]
    }
}

/// Collects all injection regions in the document
///
/// Unlike `detect_injection` which requires a specific node,
/// this function finds ALL injection regions in the entire document.
/// Used for semantic tokens to highlight all injected content.
///
/// # Arguments
/// * `root` - Root node of the document AST
/// * `text` - The document text
/// * `injection_query` - The injection query for detecting injections
///
/// # Returns
/// Vector of injection region information, or None if no query
pub fn collect_all_injections<'a>(
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
) -> Option<Vec<InjectionRegionInfo<'a>>> {
    let query = injection_query?;

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, text.as_bytes());

    // Use a map to deduplicate by content node range
    let mut injections_map = std::collections::HashMap::new();

    while let Some(match_) = matches.next() {
        for capture in iter_valid_injection_content_captures(match_, query, text) {
            if capture.node.start_byte() >= capture.node.end_byte() {
                continue;
            }
            if let Some(language) = extract_injection_language(query, match_, text) {
                let key = (capture.node.start_byte(), capture.node.end_byte());
                injections_map.entry(key).or_insert(InjectionRegionInfo {
                    language,
                    content_node: capture.node,
                    pattern_index: match_.pattern_index,
                    include_children: has_include_children_for_pattern(query, match_.pattern_index),
                });
            }
        }
    }

    // Sort by start_byte (primary) and end_byte (secondary) to ensure deterministic ordering
    let mut injections: Vec<_> = injections_map.into_values().collect();
    injections.sort_by_key(|r| (r.content_node.start_byte(), r.content_node.end_byte()));
    Some(injections)
}

/// Detects injection and returns both the language and the content node
/// Also returns the pattern index of the innermost injection for offset lookups
pub fn detect_injection<'a>(
    node: &Node<'a>,
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
    base_language: &str,
) -> Option<(Vec<String>, Node<'a>, usize)> {
    let injections = collect_injection_regions(node, root, text, injection_query)?;

    if injections.is_empty() {
        return None;
    }

    // Sort injections by their range (outer to inner)
    let mut sorted_injections = injections;
    sorted_injections.sort_by(|a, b| {
        // Sort by start byte (ascending), then by end byte (descending)
        // This ensures outer injections come before inner ones
        a.0.cmp(&b.0).then(b.1.cmp(&a.1))
    });

    // Build the language hierarchy from outermost to innermost
    let mut hierarchy = vec![base_language.to_string()];
    for (_, _, lang, _, _) in &sorted_injections {
        hierarchy.push(lang.clone());
    }

    // Return the innermost content node and its pattern index
    let (_, _, _, innermost_node, pattern_index) = sorted_injections.last().cloned()?;

    Some((hierarchy, innermost_node, pattern_index))
}

/// Represents an injection region with its metadata
type InjectionRegion<'a> = (usize, usize, String, Node<'a>, usize);

/// Collects all injection regions that contain the given node
/// Returns tuples of (start_byte, end_byte, language, content_node, pattern_index)
fn collect_injection_regions<'a>(
    node: &Node<'a>,
    root: &Node<'a>,
    text: &str,
    injection_query: Option<&Query>,
) -> Option<Vec<InjectionRegion<'a>>> {
    let query = injection_query?;

    // Run the query on the entire tree
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, text.as_bytes());

    // Collect all injection regions that contain our node
    // Use a map to deduplicate by node range (start, end)
    let mut injections_map = std::collections::HashMap::new();

    while let Some(match_) = matches.next() {
        if let Some((content_node, language, pattern_index)) =
            extract_content_and_language(node, match_, query, text)
        {
            let key = (content_node.start_byte(), content_node.end_byte());

            // Only keep the first injection for each unique range
            // This handles cases where multiple patterns match the same node
            injections_map.entry(key).or_insert((
                content_node.start_byte(),
                content_node.end_byte(),
                language,
                content_node,
                pattern_index,
            ));
        }
    }

    // Convert to vector
    let injections: Vec<_> = injections_map.into_values().collect();

    Some(injections)
}

/// Extracts the injection content node and language if the given node is within it
/// Also returns the pattern index for offset lookups
fn extract_content_and_language<'a>(
    node: &Node<'a>,
    match_: &QueryMatch<'_, 'a>,
    query: &Query,
    text: &str,
) -> Option<(Node<'a>, String, usize)> {
    for capture in iter_valid_injection_content_captures(match_, query, text) {
        let content_node = capture.node;

        // Check if our node is within this injection region
        if is_node_within(node, &content_node) {
            // Extract the injection language
            if let Some(language) = extract_injection_language(query, match_, text) {
                return Some((content_node, language, match_.pattern_index));
            }
        }
    }

    None
}

/// Find the injection region containing the given byte offset.
///
/// Returns the index and reference to the matching region, or None if the position
/// is not within any injection region.
///
/// # Arguments
/// * `injections` - All injection regions in document order
/// * `byte_offset` - The byte offset to search for
///
/// # Returns
/// Option of (index, reference to region) for use with calculate_region_id
pub fn find_injection_at_position<'a>(
    injections: &'a [InjectionRegionInfo<'a>],
    byte_offset: usize,
) -> Option<(usize, &'a InjectionRegionInfo<'a>)> {
    injections.iter().enumerate().find(|(_, inj)| {
        let start = inj.content_node.start_byte();
        let end = inj.content_node.end_byte();
        byte_offset >= start && byte_offset < end
    })
}

/// Resolved injection region with all necessary context for LSP bridge requests
pub struct ResolvedInjection {
    /// Cacheable injection region with line range information
    pub region: CacheableInjectionRegion,
    /// Stable region identifier (ULID format, 26 alphanumeric characters)
    pub region_id: String,
    /// Language of the injection content
    pub injection_language: String,
    /// Extracted virtual document content (clean, with blockquote prefixes stripped)
    pub virtual_content: String,
    /// Per-virtual-line column offsets for coordinate translation.
    /// Each entry is the UTF-16 column offset for that virtual line.
    pub line_column_offsets: Vec<u32>,
}

/// Central service for resolving injection regions at LSP positions
pub struct InjectionResolver;

impl InjectionResolver {
    /// Resolve injection region at the given byte offset
    ///
    /// This function centralizes the injection resolution logic that was previously
    /// duplicated across 9 LSP handlers (hover, completion, definition, etc.).
    ///
    /// # Lock Safety
    /// This function does not hold Document locks. All inputs (tree, text) must be
    /// pre-cloned, typically via DocumentSnapshot.
    ///
    /// # Arguments
    /// * `coordinator` - Language coordinator for resolving injection language
    /// * `tracker` - Region ID tracker for stable ULID generation
    /// * `uri` - Host document URI
    /// * `tree` - Parsed syntax tree
    /// * `text` - Document text content
    /// * `injection_query` - Query for finding injection regions
    /// * `byte_offset` - Byte offset to resolve
    ///
    /// # Returns
    /// `Some(ResolvedInjection)` if position is within an injection region,
    /// `None` otherwise.
    pub(crate) fn resolve_at_byte_offset(
        coordinator: &LanguageCoordinator,
        tracker: &RegionIdTracker,
        uri: &Url,
        tree: &Tree,
        text: &str,
        injection_query: &Query,
        byte_offset: usize,
    ) -> Option<ResolvedInjection> {
        // 1. Collect all injection regions
        let injections = collect_all_injections(&tree.root_node(), text, Some(injection_query))?;

        // 2. Find injection region containing this position
        let (_region_index, region) = find_injection_at_position(&injections, byte_offset)?;

        // 3. Calculate stable ULID-based region_id (Phase 2: position-based)
        let region_id = Self::calculate_region_id(tracker, uri, region);
        let region_id_str = region_id.to_string();

        // 4. Build cacheable region with line range information
        let cacheable_region =
            CacheableInjectionRegion::from_region_info(region, &region_id_str, text);

        // 5. Extract clean virtual content and per-line column offsets
        let (virtual_content, line_column_offsets) =
            extract_virtual_content_and_offsets(region, &cacheable_region, text);

        // 6. Resolve injection language using unified detection (ADR-0005)
        // This normalizes tokens like "py" -> "python" for bridge server lookup
        let resolved_language =
            Self::resolve_language(coordinator, &region.language, &virtual_content);

        Some(ResolvedInjection {
            region: cacheable_region,
            region_id: region_id_str,
            injection_language: resolved_language,
            virtual_content,
            line_column_offsets,
        })
    }

    /// Calculate a stable ULID-based region_id for an injection.
    ///
    /// Phase 2 (ADR-0019): Uses position-based key (start_byte, end_byte, kind) for ULID lookup.
    ///
    /// # Arguments
    /// * `tracker` - The region ID tracker for ULID generation/lookup
    /// * `uri` - The host document URI
    /// * `injection` - The injection region info
    ///
    /// # Returns
    /// A stable ULID that remains constant for the same position key.
    pub(crate) fn calculate_region_id(
        tracker: &RegionIdTracker,
        uri: &Url,
        injection: &InjectionRegionInfo,
    ) -> Ulid {
        tracker.get_or_create(
            uri,
            injection.content_node.start_byte(),
            injection.content_node.end_byte(),
            injection.content_node.kind(),
        )
    }

    /// Resolve injection language using the unified detection chain (ADR-0005).
    ///
    /// This normalizes raw fence identifiers (e.g., "py") to canonical language names
    /// (e.g., "python") that match bridge server configurations.
    ///
    /// Falls back to the raw identifier if no resolution is found, allowing the
    /// bridge lookup to fail gracefully with a clear error message.
    fn resolve_language(
        coordinator: &LanguageCoordinator,
        raw_identifier: &str,
        content: &str,
    ) -> String {
        coordinator
            .detect_language(
                raw_identifier, // Use identifier as path for token extraction
                content,
                Some(raw_identifier), // Explicit token
                Some(raw_identifier), // Also try as languageId
            )
            .unwrap_or_else(|| raw_identifier.to_string())
    }

    /// Resolve all injection regions in a document.
    ///
    /// This is used for whole-document operations like document_link that need
    /// to iterate over all injection regions rather than finding one at a position.
    ///
    /// # Lock Safety
    /// This function does not hold Document locks. All inputs (tree, text) must be
    /// pre-cloned, typically via DocumentSnapshot.
    ///
    /// # Arguments
    /// * `coordinator` - Language coordinator for resolving injection language
    /// * `tracker` - Region ID tracker for stable ULID generation
    /// * `uri` - Host document URI
    /// * `tree` - Parsed syntax tree
    /// * `text` - Document text content
    /// * `injection_query` - Query for finding injection regions
    ///
    /// # Returns
    /// Vector of resolved injections, may be empty if no injections found.
    pub(crate) fn resolve_all(
        coordinator: &LanguageCoordinator,
        tracker: &RegionIdTracker,
        uri: &Url,
        tree: &Tree,
        text: &str,
        injection_query: &Query,
    ) -> Vec<ResolvedInjection> {
        // Collect all injection regions
        let Some(injections) =
            collect_all_injections(&tree.root_node(), text, Some(injection_query))
        else {
            return Vec::new();
        };

        // Resolve each injection
        injections
            .iter()
            .map(|region| {
                let region_id = Self::calculate_region_id(tracker, uri, region);
                let region_id_str = region_id.to_string();
                let cacheable_region =
                    CacheableInjectionRegion::from_region_info(region, &region_id_str, text);

                let (virtual_content, line_column_offsets) =
                    extract_virtual_content_and_offsets(region, &cacheable_region, text);

                // Resolve injection language using unified detection (ADR-0005)
                let resolved_language =
                    Self::resolve_language(coordinator, &region.language, &virtual_content);

                ResolvedInjection {
                    region: cacheable_region,
                    region_id: region_id_str,
                    injection_language: resolved_language,
                    virtual_content,
                    line_column_offsets,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tree_sitter::Parser;

    #[test]
    fn test_parse_offset_directive_for_pattern() {
        // Test that the pattern-aware function correctly returns
        // offsets only for the specific pattern

        // Create a query similar to markdown's injection.scm with multiple patterns
        let query_str = r#"
            ; Pattern 0: Raw string literals - NO OFFSET
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ; Pattern 1: Comments - HAS OFFSET
            ((line_comment) @injection.content
              (#set! injection.language "markdown")
              (#offset! @injection.content 1 0 -1 0))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Pattern 0 (raw_string_literal) has NO offset
        let offset_pattern_0 = parse_offset_directive_for_pattern(&query, 0);
        assert_eq!(offset_pattern_0, None, "Pattern 0 should have no offset");

        // Pattern 1 (line_comment) HAS offset
        let offset_pattern_1 = parse_offset_directive_for_pattern(&query, 1);
        assert_eq!(
            offset_pattern_1,
            Some(InjectionOffset::new(1, 0, -1, 0)),
            "Pattern 1 should have offset (1, 0, -1, 0)"
        );
    }

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
        let (hierarchy, _content_node, _pattern_index) = result.unwrap();

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
            result.map(|(h, _, _)| h),
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
        assert_eq!(result.map(|(h, _, _)| h), None);
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
        let (hierarchy, _, _) = result.unwrap();
        assert_eq!(hierarchy, vec!["rust", "nested_lang"]);

        // The actual deep recursion is tested through integration with refactor.rs
        // where handle_nested_injection recursively processes injections
    }

    #[test]
    fn test_duplicate_injections_same_node() {
        // Test that multiple injection patterns matching the same node
        // should only result in one injection (not nested)
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

        // Should detect only one injection (first pattern takes precedence)
        assert!(result.is_some(), "Should find injection");
        let (hierarchy, _, _) = result.unwrap();
        // Should only use the first matching pattern, not both
        assert_eq!(
            hierarchy,
            vec!["rust", "doc"],
            "Should only show first injection"
        );
    }

    // Helper function to find a node at a specific byte position
    fn find_node_at_byte<'a>(root: &Node<'a>, byte: usize) -> Option<Node<'a>> {
        root.descendant_for_byte_range(byte, byte)
    }

    #[rstest]
    #[case::non_numeric_values("foo bar baz qux", Some(super::DEFAULT_OFFSET))]
    #[case::missing_arguments("1 0", Some(super::DEFAULT_OFFSET))]
    #[case::extra_arguments("1 0 -1 0 5", Some(InjectionOffset::new(1, 0, -1, 0)))]
    #[case::mixed_valid_invalid("1 invalid -1 0", Some(super::DEFAULT_OFFSET))]
    #[case::empty_args("", Some(super::DEFAULT_OFFSET))]
    #[trace]
    fn test_offset_directive_edge_cases(
        #[case] offset_args: &str,
        #[case] expected: Option<InjectionOffset>,
    ) {
        let language = tree_sitter_rust::LANGUAGE.into();
        let query_str = if offset_args.is_empty() {
            r#"
            ((line_comment) @injection.content
              (#set! injection.language "test")
              (#offset! @injection.content))
        "#
            .to_string()
        } else {
            format!(
                r#"
            ((line_comment) @injection.content
              (#set! injection.language "test")
              (#offset! @injection.content {}))
        "#,
                offset_args
            )
        };

        let query = Query::new(&language, &query_str).expect("valid query");
        let offset = parse_offset_directive_for_pattern(&query, 0);

        assert_eq!(
            offset, expected,
            "offset_args={:?} should produce {:?}",
            offset_args, expected
        );
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
    fn test_cacheable_injection_region_contains_byte() {
        let region = CacheableInjectionRegion {
            language: "lua".to_string(),
            byte_range: 100..200,
            line_range: 5..10,
            start_column: 0,
            region_id: "test-region".to_string(),
            content_hash: 12345,
        };

        // Byte within range
        assert!(
            region.contains_byte(100),
            "Start of range should be included"
        );
        assert!(
            region.contains_byte(150),
            "Middle of range should be included"
        );
        assert!(
            region.contains_byte(199),
            "End-1 of range should be included"
        );

        // Byte outside range
        assert!(
            !region.contains_byte(99),
            "Before range should not be included"
        );
        assert!(
            !region.contains_byte(200),
            "End of range (exclusive) should not be included"
        );
        assert!(
            !region.contains_byte(300),
            "Far after range should not be included"
        );
    }

    #[test]
    fn test_cacheable_injection_region_extract_content() {
        // Simulate a Markdown document with a Rust code block
        let host_text =
            "# Title\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```\n\nMore text";
        //                0123456789...
        // The code block content starts at byte 17 (after "```rust\n") and ends at byte 54

        let region = CacheableInjectionRegion {
            language: "rust".to_string(),
            byte_range: 17..54, // "fn main() {\n    println!(\"hello\");\n}"
            line_range: 3..6,
            start_column: 0,
            region_id: "test-region".to_string(),
            content_hash: 12345,
        };

        let content = region.extract_content(host_text);
        assert_eq!(content, "fn main() {\n    println!(\"hello\");\n}\n");
    }

    // ============================================================
    // Tests for InjectionResolver region_id generation
    // ============================================================

    use crate::language::region_id_tracker::RegionIdTracker;

    fn test_uri(name: &str) -> Url {
        Url::parse(&format!("file:///test/{}.rs", name)).unwrap()
    }

    fn test_coordinator() -> LanguageCoordinator {
        LanguageCoordinator::new()
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
        let tracker = RegionIdTracker::new();
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
        );
        assert!(resolved.is_some(), "Should resolve injection");
        let region_id = resolved.unwrap().region_id;
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
        let tracker = RegionIdTracker::new();
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
        );
        let r2 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[1],
        );
        let r3 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offsets[2],
        );

        // Each should have different ULIDs (different ordinals)
        let id1 = r1.unwrap().region_id;
        let id2 = r2.unwrap().region_id;
        let id3 = r3.unwrap().region_id;
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
        let tracker = RegionIdTracker::new();
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
        );
        let r2 = InjectionResolver::resolve_at_byte_offset(
            &coordinator,
            &tracker,
            &uri,
            &tree,
            text,
            &query,
            byte_offset,
        );

        assert_eq!(
            r1.unwrap().region_id,
            r2.unwrap().region_id,
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
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: nodes[1],
                pattern_index: 0,
                include_children: false,
            },
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[2],
                pattern_index: 0,
                include_children: false,
            },
        ];

        let tracker = RegionIdTracker::new();
        let uri = test_uri("mixed");

        // Phase 2: calculate_region_id uses position-based keys (not ordinals)
        // Different positions → different ULIDs regardless of language
        let ulid_0 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[0]);
        let ulid_1 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[1]);
        let ulid_2 = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[2]);

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
        let ulid_0_again = InjectionResolver::calculate_region_id(&tracker, &uri, &injections[0]);
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
            },
            InjectionRegionInfo {
                language: "python".to_string(),
                content_node: nodes[1],
                pattern_index: 0,
                include_children: false,
            },
            InjectionRegionInfo {
                language: "lua".to_string(),
                content_node: nodes[2],
                pattern_index: 0,
                include_children: false,
            },
        ];

        // Test finding position inside first Lua block
        let lua1_byte = nodes[0].start_byte() + 1; // Inside first string
        let result = find_injection_at_position(&injections, lua1_byte);
        assert!(result.is_some(), "Should find injection at lua1 position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 0, "Should be at index 0");
        assert_eq!(region.language, "lua", "Should be lua region");

        // Test finding position inside Python block
        let py_byte = nodes[1].start_byte() + 1;
        let result = find_injection_at_position(&injections, py_byte);
        assert!(result.is_some(), "Should find injection at python position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 1, "Should be at index 1");
        assert_eq!(region.language, "python", "Should be python region");

        // Test finding position inside second Lua block
        let lua2_byte = nodes[2].start_byte() + 1;
        let result = find_injection_at_position(&injections, lua2_byte);
        assert!(result.is_some(), "Should find injection at lua2 position");
        let (idx, region) = result.unwrap();
        assert_eq!(idx, 2, "Should be at index 2");
        assert_eq!(region.language, "lua", "Should be lua region");

        // Test position outside all injections
        let outside_byte = 5; // Position before any string
        let result = find_injection_at_position(&injections, outside_byte);
        assert!(
            result.is_none(),
            "Should not find injection outside regions"
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

    // ============================================================
    // Tests for line_range edge cases in CacheableInjectionRegion
    // ============================================================

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
    fn test_has_include_children_for_pattern_without_property() {
        // Pattern 0 has NO #set! injection.include-children
        let query_str = r#"
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            !has_include_children_for_pattern(&query, 0),
            "Pattern without #set! injection.include-children should return false"
        );
    }

    #[test]
    fn test_has_include_children_for_pattern_with_property() {
        // Pattern has #set! injection.include-children (value-less property)
        let query_str = r#"
            ((line_comment) @injection.content
              (#set! injection.language "html")
              (#set! injection.include-children))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            has_include_children_for_pattern(&query, 0),
            "Pattern with #set! injection.include-children should return true"
        );
    }

    #[test]
    fn test_has_include_children_multi_pattern() {
        // Two patterns: first without, second with include-children
        let query_str = r#"
            ; Pattern 0: NO include-children
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ; Pattern 1: HAS include-children
            ((line_comment) @injection.content
              (#set! injection.language "yaml")
              (#set! injection.include-children))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            !has_include_children_for_pattern(&query, 0),
            "Pattern 0 should not have include-children"
        );
        assert!(
            has_include_children_for_pattern(&query, 1),
            "Pattern 1 should have include-children"
        );
    }

    #[test]
    fn test_compute_included_ranges_returns_none_when_include_children() {
        // When include_children is true, all content is included (no restriction)
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let root = tree.root_node();

        assert!(
            compute_included_ranges(&root, true).is_none(),
            "include_children=true should return None"
        );
    }

    #[test]
    fn test_compute_included_ranges_returns_none_for_no_named_children() {
        // A leaf node (identifier) has no named children
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("x", None).unwrap();
        let root = tree.root_node();

        // Find identifier leaf node
        let ident = root
            .named_descendant_for_byte_range(0, 1)
            .expect("should find node");
        assert_eq!(ident.named_child_count(), 0);

        assert!(
            compute_included_ranges(&ident, false).is_none(),
            "Node with no named children should return None"
        );
    }

    #[test]
    fn test_compute_included_ranges_computes_gap_ranges() {
        // Use a node that has named children with gaps between them.
        // In Rust, a function_item has named children (name, parameters, body)
        // with gaps (the "fn" keyword, whitespace, etc.)
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse("fn main() {}", None).unwrap();
        let root = tree.root_node();

        // function_item is the top-level node
        let func = root.named_child(0).expect("should have function_item");
        let func_start = func.start_byte();
        let func_size = func.end_byte() - func_start;

        let ranges = compute_included_ranges(&func, false);
        let ranges = ranges.expect("Node with named children should produce gap ranges");
        assert!(
            !ranges.is_empty(),
            "Should have at least one gap range between named children"
        );

        // Collect named children byte ranges (relative to func start)
        let mut tree_cursor = func.walk();
        let child_ranges: Vec<(usize, usize)> = func
            .named_children(&mut tree_cursor)
            .map(|c| (c.start_byte() - func_start, c.end_byte() - func_start))
            .collect();

        // Gap ranges must not overlap any named child
        for r in &ranges {
            assert!(
                r.start_byte < r.end_byte,
                "Range should be non-empty: {r:?}"
            );
            assert!(r.end_byte <= func_size, "Range exceeds node bounds: {r:?}");
            for (cs, ce) in &child_ranges {
                assert!(
                    r.end_byte <= *cs || r.start_byte >= *ce,
                    "Gap range {r:?} overlaps named child [{cs}, {ce})"
                );
            }
        }

        // Gap ranges + named child ranges should cover the entire node
        let mut covered = vec![false; func_size];
        for r in &ranges {
            for c in covered[r.start_byte..r.end_byte].iter_mut() {
                *c = true;
            }
        }
        for (cs, ce) in &child_ranges {
            for c in covered[*cs..*ce].iter_mut() {
                *c = true;
            }
        }
        assert!(
            covered.iter().all(|&b| b),
            "Gaps + children should cover the entire node"
        );
    }

    #[test]
    fn test_blockquote_bridge_content_strips_prefixes() {
        // Verify that extract_clean_content strips blockquote prefixes so that
        // downstream language servers receive parseable code.
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, _start_column, included_ranges) = blockquote_injection_data(text);

        let clean = extract_clean_content(text, byte_range, included_ranges.as_deref());
        assert!(
            !clean.contains("> "),
            "Clean content should NOT contain blockquote prefixes: {:?}",
            clean
        );
        assert!(
            clean.contains("local x = 1"),
            "Clean content should contain the code: {:?}",
            clean
        );
        assert!(
            clean.contains("local y = 2"),
            "Clean content should contain the code: {:?}",
            clean
        );
    }

    // ============================================================
    // Tests for extract_clean_content and compute_line_column_offsets
    // ============================================================

    /// Helper to set up a markdown parser and injection query, parse text,
    /// and return the first injection's content node byte range and included ranges.
    fn blockquote_injection_data(
        text: &str,
    ) -> (std::ops::Range<usize>, u32, Option<Vec<tree_sitter::Range>>) {
        let mut parser = Parser::new();
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser
            .set_language(&md_language)
            .expect("load markdown grammar");

        let injection_query_str = r#"
            (fenced_code_block
              (info_string (language) @injection.language)
              (code_fence_content) @injection.content)
        "#;
        let injection_query =
            Query::new(&md_language, injection_query_str).expect("valid injection query");

        let tree = parser.parse(text, None).expect("parse markdown");
        let root = tree.root_node();

        let injections = collect_all_injections(&root, text, Some(&injection_query))
            .expect("Should find injections");
        assert!(!injections.is_empty(), "Should find at least one injection");

        let region = &injections[0];
        let byte_range = region.content_node.byte_range();
        let included_ranges =
            compute_included_ranges(&region.content_node, region.include_children);

        // Compute start_column (UTF-16)
        let byte_column = region.content_node.start_position().column;
        let line_start_byte = region.content_node.start_byte() - byte_column;
        let line_prefix = &text[line_start_byte..region.content_node.start_byte()];
        let start_column = line_prefix.encode_utf16().count() as u32;

        (byte_range, start_column, included_ranges)
    }

    #[test]
    fn extract_clean_content_without_ranges_returns_full_text() {
        // When called with None for included_ranges, returns the full content slice
        let text = "hello world";
        let clean = extract_clean_content(text, 0..text.len(), None);
        assert_eq!(clean, text);
    }

    #[test]
    fn extract_clean_content_with_ranges_strips_prefixes() {
        // Blockquote case: included_ranges present → only gap content
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, _start_column, included_ranges) = blockquote_injection_data(text);
        assert!(
            included_ranges.is_some(),
            "Blockquote should have included ranges"
        );

        let clean = extract_clean_content(text, byte_range, included_ranges.as_deref());
        assert!(
            !clean.contains("> "),
            "Clean content should not contain blockquote prefixes: {:?}",
            clean
        );
        assert!(
            clean.contains("local x = 1"),
            "Clean content should contain the code: {:?}",
            clean
        );
        assert!(
            clean.contains("local y = 2"),
            "Clean content should contain the code: {:?}",
            clean
        );
    }

    #[test]
    fn compute_line_column_offsets_without_ranges_returns_start_column() {
        // When called with None for included_ranges, returns vec![start_column]
        let offsets = compute_line_column_offsets("hello", 0..5, 4, None);
        assert_eq!(offsets, vec![4]);
    }

    #[test]
    fn compute_line_column_offsets_with_ranges_returns_per_line() {
        // Blockquote case: "> " prefix on each line → 2 UTF-16 units per line
        let text = "> ```lua\n> local x = 1\n> local y = 2\n> ```\n";
        let (byte_range, start_column, included_ranges) = blockquote_injection_data(text);
        assert!(included_ranges.is_some());

        let offsets =
            compute_line_column_offsets(text, byte_range, start_column, included_ranges.as_deref());
        // Virtual lines 0 and 1 contain actual code after "> " prefixes.
        // Line 2 may be trailing content (the "> " before "```") — its offset
        // is 0 because there's no gap range starting on that line.
        assert!(
            offsets.len() >= 2,
            "Should have at least 2 line offsets: {:?}",
            offsets
        );
        assert_eq!(
            offsets[0], 2,
            "Line 0 should have offset 2 (for '> ' prefix)"
        );
        assert_eq!(
            offsets[1], 2,
            "Line 1 should have offset 2 (for '> ' prefix)"
        );
    }

    /// Tree-sitter assigns column positions from Range.start_point, not byte offset.
    ///
    /// When `set_included_ranges` is called with ranges whose `start_point.column = 2`,
    /// tree-sitter reports parsed nodes at column 2 — not column 0 — even though the
    /// bytes start at offset 2 within the raw text.
    ///
    /// This is the invariant that makes blockquote injection work correctly:
    /// `compute_included_ranges` builds ranges with `start_point.column = prefix_len`
    /// (e.g. 2 for `> `), so injected keywords appear at their true host column.
    #[test]
    fn test_parse_with_included_ranges_preserves_start_point_column() {
        // Simulate "> let x = 1;\n> let y = 2;\n" with included_ranges skipping `> `
        // Line 0: "> let x = 1;\n" = 13 bytes (0..13, \n at byte 12)
        // Line 1: "> let y = 2;\n" = 13 bytes (13..26, \n at byte 25)
        // Content ranges (skipping 2-byte `> ` prefix on each line):
        //   Range 0: bytes  2..13 = "let x = 1;\n", start_point {row:0, col:2}
        //   Range 1: bytes 15..26 = "let y = 2;\n", start_point {row:1, col:2}
        let content_text = "> let x = 1;\n> let y = 2;\n";
        let included_ranges = vec![
            tree_sitter::Range {
                start_byte: 2,
                end_byte: 13,
                start_point: tree_sitter::Point { row: 0, column: 2 },
                end_point: tree_sitter::Point { row: 1, column: 0 },
            },
            tree_sitter::Range {
                start_byte: 15,
                end_byte: 26,
                start_point: tree_sitter::Point { row: 1, column: 2 },
                end_point: tree_sitter::Point { row: 2, column: 0 },
            },
        ];

        let rust_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&rust_language).unwrap();
        let tree = parse_with_ranges(
            &mut parser,
            content_text,
            Some(&included_ranges),
            "test",
            "rust",
        )
        .expect("should parse");

        let query = Query::new(&rust_language, "(let_declaration) @decl").unwrap();
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), content_text.as_bytes());

        let mut let_columns: Vec<usize> = Vec::new();
        while let Some(m) = matches.next() {
            for c in m.captures {
                let node = c.node;
                let mut walk = node.walk();
                for child in node.children(&mut walk) {
                    if child.kind() == "let" {
                        let_columns.push(child.start_position().column);
                    }
                }
            }
        }

        assert_eq!(
            let_columns,
            vec![2, 2],
            "Both 'let' keywords should be at column 2 (from Range.start_point), not 0"
        );
    }

    // ─── Tests for sub_select_included_ranges ────────────────────────────

    /// Helper to build a tree_sitter::Range for test fixtures.
    fn make_range(
        start_byte: usize,
        end_byte: usize,
        start_row: usize,
        start_col: usize,
        end_row: usize,
        end_col: usize,
    ) -> tree_sitter::Range {
        tree_sitter::Range {
            start_byte,
            end_byte,
            start_point: tree_sitter::Point {
                row: start_row,
                column: start_col,
            },
            end_point: tree_sitter::Point {
                row: end_row,
                column: end_col,
            },
        }
    }

    #[test]
    fn test_sub_select_included_ranges_basic() {
        // Parent has 3 ranges spanning 3 rows (simulating "> " prefix on each line).
        // Each line is ">" (1 byte) + " " (1 byte) + content.
        // Line 0: "> abc\n"  → bytes 0..6, gap at 2..6, row 0 col 2
        // Line 1: "> def\n"  → bytes 6..12, gap at 8..12, row 1 col 2
        // Line 2: "> ghi\n"  → bytes 12..18, gap at 14..18, row 2 col 2
        let parent_ranges = vec![
            make_range(2, 6, 0, 2, 0, 6),
            make_range(8, 12, 1, 2, 1, 6),
            make_range(14, 18, 2, 2, 2, 6),
        ];

        // Nested region covers rows 1-2 (bytes 8..18 in parent coordinates)
        let result = sub_select_included_ranges(&parent_ranges, 8, 18);

        assert!(
            result.is_some(),
            "Should return Some for overlapping ranges"
        );
        let ranges = result.unwrap();
        assert_eq!(ranges.len(), 2, "Should have 2 ranges for rows 1-2");

        // First range: re-relativized from parent byte 8..12 → nested byte 0..4
        // First range column is 0 (prefix bytes are outside nested content_text)
        assert_eq!(ranges[0].start_byte, 0);
        assert_eq!(ranges[0].end_byte, 4);
        assert_eq!(ranges[0].start_point.row, 0);
        assert_eq!(ranges[0].start_point.column, 0); // first range: no prefix in nested content
        assert_eq!(ranges[0].end_point.row, 0);
        assert_eq!(ranges[0].end_point.column, 6);

        // Second range: re-relativized from parent byte 14..18 → nested byte 6..10
        assert_eq!(ranges[1].start_byte, 6);
        assert_eq!(ranges[1].end_byte, 10);
        assert_eq!(ranges[1].start_point.row, 1);
        assert_eq!(ranges[1].start_point.column, 2);
        assert_eq!(ranges[1].end_point.row, 1);
        assert_eq!(ranges[1].end_point.column, 6);
    }

    #[test]
    fn test_sub_select_included_ranges_no_overlap() {
        // Parent ranges span bytes 0..6
        let parent_ranges = vec![make_range(2, 6, 0, 2, 0, 6)];

        // Nested region is completely outside parent ranges
        let result = sub_select_included_ranges(&parent_ranges, 10, 20);

        assert!(
            result.is_none(),
            "Should return None for non-overlapping ranges"
        );
    }

    #[test]
    fn test_sub_select_included_ranges_partial_overlap() {
        // Parent range: bytes 2..10 on row 0
        let parent_ranges = vec![make_range(2, 10, 0, 2, 0, 10)];

        // Nested region starts at byte 5 (mid-range) and ends at byte 15 (past range)
        let result = sub_select_included_ranges(&parent_ranges, 5, 15);

        assert!(
            result.is_some(),
            "Should return Some for partially overlapping range"
        );
        let ranges = result.unwrap();
        assert_eq!(ranges.len(), 1);

        // Clipped: start clamped to 5, end clamped to 10
        // Re-relativized: 5-5=0 start, 10-5=5 end
        assert_eq!(ranges[0].start_byte, 0);
        assert_eq!(ranges[0].end_byte, 5);
        assert_eq!(ranges[0].start_point.row, 0);
        assert_eq!(ranges[0].start_point.column, 0); // first range: column zeroed
    }
}
