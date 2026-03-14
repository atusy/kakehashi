//! Token collection from tree-sitter queries.
//!
//! This module handles the collection of raw tokens from a single document's
//! highlight query, including multiline token handling and byte-to-UTF16 conversion.

use crate::config::CaptureMappings;
use crate::text::position::byte_to_utf16_col;
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

use super::legend::{CaptureResult, resolve_capture};

/// Check whether a node is strictly contained within any exclusion range.
///
/// A node is excluded only if it is **properly inside** a range — meaning fully
/// contained but NOT exactly equal. Nodes whose range exactly equals an exclusion
/// range are preserved; conflicts at the same position are resolved downstream
/// by the sweep line algorithm in `finalize_tokens()`.
fn is_in_exclusion_range(node: &Node, ranges: &[(usize, usize)]) -> bool {
    let node_start = node.start_byte();
    let node_end = node.end_byte();
    ranges.iter().any(|&(range_start, range_end)| {
        node_start >= range_start
            && node_end <= range_end
            && (node_start != range_start || node_end != range_end)
    })
}

/// Extract the `#set! priority N` value for a pattern, defaulting to 100.
///
/// Only pattern-level settings (where `capture_id` is `None`) are considered.
/// Per-capture priority (`capture_id: Some(_)`) is ignored — no shipped queries use it.
fn parse_priority_for_pattern(query: &Query, pattern_index: usize) -> u32 {
    const DEFAULT_PRIORITY: u32 = 100;

    for prop in query.property_settings(pattern_index) {
        if prop.key.as_ref() == "priority"
            && prop.capture_id.is_none()
            && let Some(ref value) = prop.value
        {
            match value.parse::<u32>() {
                Ok(n) => return n,
                Err(_) => {
                    log::warn!(
                        target: "kakehashi::semantic",
                        "Invalid #set! priority value {:?} in pattern {}, using default {}",
                        value, pattern_index, DEFAULT_PRIORITY
                    );
                }
            }
        }
    }
    DEFAULT_PRIORITY
}

/// The semantic role of a collected token.
///
/// Mirrors the relevant variants of [`CaptureResult`] but lives on the
/// collected token, encoding token roles as distinct variants rather than
/// magic-string conventions.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    /// Normal semantic token with a mapped type name (e.g., "keyword", "variable.readonly").
    Mapped(String),
    /// Transparent breakpoint-only token — not emitted as a semantic token.
    Transparent,
    /// `@none` capture — punches holes in parent tokens during pre-processing.
    NoneCapture,
}

impl TokenKind {
    /// Returns `true` for tokens that will be emitted as semantic tokens in the LSP response.
    pub(crate) fn is_emitted(&self) -> bool {
        matches!(self, TokenKind::Mapped(_))
    }
}

/// Represents a token before delta encoding with all position information.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawToken {
    /// 0-indexed line number in the host document
    pub line: usize,
    /// UTF-16 column position within the line
    pub column: usize,
    /// Length in UTF-16 code units
    pub length: usize,
    /// The semantic role of this token.
    pub kind: TokenKind,
    /// Injection depth (0 = host document)
    pub depth: usize,
    /// Index of the query pattern that produced this token.
    /// Within a single query, later patterns (higher index) are more specific
    /// and override earlier ones when priority, depth, and node_byte_len all tie.
    pub pattern_index: usize,
    /// Priority from `#set! priority N` directive (default 100).
    /// Higher values win during overlap resolution.
    pub priority: u32,
    /// Byte length of the tree-sitter node that produced this token.
    ///
    /// Encodes node specificity (smaller nodes are more specific than larger
    /// ancestor nodes). Used in two places:
    /// 1. Sweep-line overlap resolution: smaller nodes win over larger nodes
    ///    at the same priority and depth (via inverse comparison in `token_priority`)
    /// 2. `@none` pre-processing: determines parent/child relationships when
    ///    splitting tokens around `@none` regions
    pub node_byte_len: usize,
}

impl RawToken {
    /// Create a new token with the same identity (kind, depth, pattern_index,
    /// priority, node_byte_len) but at a different position and length.
    pub(crate) fn with_span(&self, line: usize, column: usize, length: usize) -> Self {
        Self {
            line,
            column,
            length,
            ..self.clone()
        }
    }
}

/// Represents the line/column boundaries of an injection region in the host document.
///
/// Used to exclude host tokens that fall inside active injection regions during finalization.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ActiveInjectionBounds {
    /// Start line (0-indexed)
    pub start_line: usize,
    /// Start column (UTF-16)
    pub start_col: usize,
    /// End line (0-indexed)
    pub end_line: usize,
    /// End column (UTF-16)
    pub end_col: usize,
}

/// Calculate byte offsets for a line within a multiline token.
///
/// This helper computes the start and end byte positions for a specific line (row)
/// within a multiline token, handling both host document and injected content coordinates.
///
/// # Arguments
/// * `row` - The current row being processed (relative to content)
/// * `start_pos` - Token start position in content coordinates
/// * `end_pos` - Token end position in content coordinates
/// * `content_start_col` - Column offset where injection starts in host line (0 for host content)
/// * `content_line_len` - Length of the content line at this row
///
/// # Returns
/// Tuple of (line_start_byte, line_end_byte) in host document coordinates
fn calculate_line_byte_offsets(
    row: usize,
    start_pos: tree_sitter::Point,
    end_pos: tree_sitter::Point,
    content_start_col: usize,
    content_line_len: usize,
    prefix_byte_widths: &[usize],
) -> (usize, usize) {
    // For blockquote injections, continuation lines have a byte prefix (e.g., "> ")
    // that shifts the actual content start. For normal injections this is 0.
    let prefix_width = prefix_byte_widths.get(row).copied().unwrap_or(0);

    // Calculate start byte offset for this line
    let line_start = if row == start_pos.row {
        if row == 0 {
            content_start_col + start_pos.column
        } else {
            start_pos.column
        }
    } else {
        // Continuation lines start after the prefix (e.g., after "> ")
        prefix_width
    };

    // Calculate end byte offset for this line
    let line_end = if row == end_pos.row {
        if row == 0 {
            content_start_col + end_pos.column
        } else {
            end_pos.column
        }
    } else {
        // Non-final lines: end at injected content's line end (not host line end)
        if row == 0 {
            content_start_col + content_line_len
        } else {
            content_line_len
        }
    };

    (line_start, line_end)
}

/// Compute effective per-line byte prefix widths for a multiline token.
///
/// For both host-level and injection-level tokens, detects **structural prefix
/// children** — named descendants that start at or before the line-leading prefix
/// boundary, span a single line, and end after the boundary — to prevent tokens
/// from spanning line-leading prefixes (e.g., blockquote `> ` markers).
///
/// For host-level tokens (`prefix_byte_widths` is empty), the prefix boundary is
/// column 0. For injection-level tokens (`prefix_byte_widths` is non-empty), the
/// boundary is the outer prefix width for each row.
///
/// A **relative inner-prefix width bound** (`inner_prefix_max = start_col - outer_at_start_row`)
/// is applied uniformly to all rows to prevent false positives. A candidate structural
/// prefix on row R is accepted only when its inner width `(ce.column - row_outer)`
/// does not exceed this bound. This correctly identifies blockquote `> ` continuations
/// (same inner width as the start row's inner prefix) while rejecting content nodes
/// like Python `string_end """` that end further past the outer boundary.
fn effective_prefix_widths(node: &Node, prefix_byte_widths: &[usize]) -> Vec<usize> {
    let start_col = node.start_position().column;
    let start_row = node.start_position().row;

    // For host-level tokens starting at col 0 with no outer prefix: no prefix to detect.
    if start_col == 0 && prefix_byte_widths.is_empty() {
        return Vec::new();
    }

    // Start with the outer prefix widths (empty for host-level tokens).
    let mut prefix_widths = prefix_byte_widths.to_vec();

    // The maximum inner-prefix width (relative to the outer prefix boundary) is
    // determined by the node's start row: `start_col - outer_at_start_row`.
    // Structural prefix children on ALL rows (start and continuation) must have
    // inner width ≤ this bound. This prevents content nodes that happen to start
    // at the outer prefix boundary (e.g., Python `string_end """`) from being
    // mistaken for structural prefixes.
    let row_0_outer = prefix_byte_widths.get(start_row).copied().unwrap_or(0);
    let inner_prefix_max = start_col.saturating_sub(row_0_outer);

    if inner_prefix_max == 0 {
        // No inner prefix possible (node starts exactly at the outer prefix boundary).
        return prefix_widths;
    }

    // Walk ALL named descendants (not just direct children) to find structural
    // prefix nodes. In Markdown, block_continuation nodes may be nested inside
    // intermediate nodes like code_fence_content or atx_heading.
    //
    // A structural prefix child on row R must:
    //   - start at or before the outer prefix boundary for row R
    //   - span exactly one line
    //   - end after the outer prefix boundary
    //   - have inner width (ce.column - row_outer) ≤ inner_prefix_max
    //     (guards against content nodes starting at the boundary on any row)
    let mut cursor = node.walk();
    let mut descended = cursor.goto_first_child();
    while descended {
        let child = cursor.node();
        if child.is_named() {
            let cs = child.start_position();
            let ce = child.end_position();
            let row_outer = prefix_byte_widths.get(cs.row).copied().unwrap_or(0);
            if cs.column <= row_outer
                && ce.row == cs.row
                && ce.column > row_outer
                && (ce.column - row_outer) <= inner_prefix_max
            {
                let row = cs.row;
                if row >= prefix_widths.len() {
                    prefix_widths.resize(row + 1, 0);
                }
                if prefix_widths[row] < ce.column {
                    prefix_widths[row] = ce.column;
                }
            }
        }
        // Depth-first traversal: try child, then sibling, then backtrack
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                descended = false;
                break;
            }
        }
    }
    prefix_widths
}

/// Collect tokens from a single document's highlight query (no injection processing).
///
/// This is the common logic shared by both pool-based and local-parser-based
/// recursive functions. It processes the given query against the tree and
/// maps positions from content-local coordinates to host document coordinates.
///
/// # Multiline Token Handling
///
/// When `supports_multiline` is true (client declares `multilineTokenSupport`),
/// tokens spanning multiple lines are emitted as-is per LSP 3.16.0+ spec.
///
/// When `supports_multiline` is false, multiline tokens are split into per-line
/// tokens for compatibility with clients that don't support multiline tokens.
#[allow(clippy::too_many_arguments)]
pub(super) fn collect_host_tokens(
    text: &str,
    tree: &Tree,
    query: &Query,
    filetype: Option<&str>,
    capture_mappings: Option<&CaptureMappings>,
    host_text: &str,
    host_lines: &[&str],
    content_start_byte: usize,
    depth: usize,
    supports_multiline: bool,
    exclusion_ranges: &[(usize, usize)],
    prefix_byte_widths: &[usize],
    all_tokens: &mut Vec<RawToken>,
) {
    // Validate content_start_byte is within bounds to prevent slice panics
    // This can happen during concurrent edits when document text shortens
    if content_start_byte > host_text.len() {
        return;
    }

    // Calculate position mapping from content-local to host document
    let content_start_line = if content_start_byte == 0 {
        0
    } else {
        host_text[..content_start_byte]
            .chars()
            .filter(|c| *c == '\n')
            .count()
    };

    let content_start_col = if content_start_byte == 0 {
        0
    } else {
        let last_newline = host_text[..content_start_byte].rfind('\n');
        match last_newline {
            Some(pos) => content_start_byte - pos - 1,
            None => content_start_byte,
        }
    };

    // Split content text into lines for byte offset calculations
    let content_lines: Vec<&str> = text.lines().collect();

    // Collect tokens from this document's highlight query
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());

    while let Some(m) = matches.next() {
        let priority = parse_priority_for_pattern(query, m.pattern_index);
        let filtered_captures = crate::language::filter_captures(query, m, text);

        for c in filtered_captures {
            let node = c.node;
            let start_pos = node.start_position();
            let end_pos = node.end_position();

            // Node byte length for specificity: smaller nodes win in sweep line
            let node_byte_len = node.end_byte() - node.start_byte();

            // Check if this is a single-line token or trailing newline case
            let is_single_line = start_pos.row == end_pos.row;
            let is_trailing_newline = end_pos.row == start_pos.row + 1 && end_pos.column == 0;

            // Get the mapped capture name early to avoid repeated mapping
            let capture_name = &query.capture_names()[c.index as usize];
            let kind = match resolve_capture(capture_name, filetype, capture_mappings) {
                CaptureResult::Suppressed => continue,
                CaptureResult::Mapped(s) => TokenKind::Mapped(s),
                CaptureResult::Transparent => TokenKind::Transparent,
                CaptureResult::NoneCapture => TokenKind::NoneCapture,
            };

            // Skip captures that fall within a child injection region
            if is_in_exclusion_range(&node, exclusion_ranges) {
                continue;
            }

            if is_single_line || is_trailing_newline {
                // Single-line token: emit as before
                let host_line = content_start_line + start_pos.row;
                let host_line_text = host_lines.get(host_line).unwrap_or(&"");

                let byte_offset_in_host = if start_pos.row == 0 {
                    content_start_col + start_pos.column
                } else {
                    start_pos.column
                };
                let start_utf16 = byte_to_utf16_col(host_line_text, byte_offset_in_host);

                // For trailing newline case, use the line length as end position
                let end_byte_offset_in_host = if is_trailing_newline {
                    host_line_text.len()
                } else if start_pos.row == 0 {
                    content_start_col + end_pos.column
                } else {
                    end_pos.column
                };
                let end_utf16 = byte_to_utf16_col(host_line_text, end_byte_offset_in_host);

                all_tokens.push(RawToken {
                    line: host_line,
                    column: start_utf16,
                    length: end_utf16 - start_utf16,
                    kind,
                    depth,
                    pattern_index: m.pattern_index,
                    priority,
                    node_byte_len,
                });
            } else {
                // Compute effective prefix widths: injection-level widths
                // are passed in; HOST-level nodes detect structural prefix
                // children to avoid spanning line-leading prefixes.
                let prefix_widths = effective_prefix_widths(&node, prefix_byte_widths);

                if supports_multiline && prefix_widths.is_empty() {
                    // Multiline token with client support AND no prefix widths:
                    // emit a single token spanning multiple lines.
                    let host_start_line = content_start_line + start_pos.row;
                    let host_end_line = content_start_line + end_pos.row;

                    let host_start_line_text = host_lines.get(host_start_line).unwrap_or(&"");
                    let start_byte_offset = if start_pos.row == 0 {
                        content_start_col + start_pos.column
                    } else {
                        start_pos.column
                    };
                    let start_utf16 = byte_to_utf16_col(host_start_line_text, start_byte_offset);

                    let mut total_length_utf16 = 0usize;
                    for row in start_pos.row..=end_pos.row {
                        let host_row = content_start_line + row;
                        let line_text = host_lines.get(host_row).unwrap_or(&"");
                        let content_line_len = content_lines.get(row).map(|l| l.len()).unwrap_or(0);

                        let (line_start, line_end) = calculate_line_byte_offsets(
                            row,
                            start_pos,
                            end_pos,
                            content_start_col,
                            content_line_len,
                            &prefix_widths,
                        );

                        let line_start_utf16 = byte_to_utf16_col(line_text, line_start);
                        let line_end_utf16 = byte_to_utf16_col(line_text, line_end);
                        total_length_utf16 += line_end_utf16 - line_start_utf16;

                        if row < end_pos.row {
                            total_length_utf16 += 1;
                        }
                    }

                    log::trace!(
                        target: "kakehashi::semantic",
                        "[MULTILINE_TOKEN] capture={} lines={}..{} host_lines={}..{} length={}",
                        capture_name, start_pos.row, end_pos.row,
                        host_start_line, host_end_line, total_length_utf16
                    );

                    all_tokens.push(RawToken {
                        line: host_start_line,
                        column: start_utf16,
                        length: total_length_utf16,
                        kind,
                        depth,
                        pattern_index: m.pattern_index,
                        priority,
                        node_byte_len,
                    });
                } else {
                    // Either no multiline support OR prefix widths present:
                    // split into per-line tokens.
                    for row in start_pos.row..=end_pos.row {
                        let host_row = content_start_line + row;
                        let host_line_text = host_lines.get(host_row).unwrap_or(&"");
                        let content_line_len = content_lines.get(row).map(|l| l.len()).unwrap_or(0);

                        let (line_start_byte, line_end_byte) = calculate_line_byte_offsets(
                            row,
                            start_pos,
                            end_pos,
                            content_start_col,
                            content_line_len,
                            &prefix_widths,
                        );

                        let start_utf16 = byte_to_utf16_col(host_line_text, line_start_byte);
                        let end_utf16 = byte_to_utf16_col(host_line_text, line_end_byte);

                        if end_utf16 > start_utf16 {
                            all_tokens.push(RawToken {
                                line: host_row,
                                column: start_utf16,
                                length: end_utf16 - start_utf16,
                                kind: kind.clone(),
                                depth,
                                pattern_index: m.pattern_index,
                                priority,
                                node_byte_len,
                            });
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TokenKind::is_emitted tests ──────────────────────────────────

    #[test]
    fn is_emitted_returns_true_for_mapped() {
        assert!(TokenKind::Mapped("keyword".to_string()).is_emitted());
    }

    #[test]
    fn is_emitted_returns_false_for_transparent() {
        assert!(!TokenKind::Transparent.is_emitted());
    }

    #[test]
    fn is_emitted_returns_false_for_none_capture() {
        assert!(!TokenKind::NoneCapture.is_emitted());
    }

    // ── is_in_exclusion_range tests ──────────────────────────────────

    fn parse_with_language(text: &str, language: tree_sitter::Language) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&language).unwrap();
        parser.parse(text, None).unwrap()
    }

    /// Helper: parse `text` with the given language and return the root node's
    /// first child (or root itself) for exclusion-range testing.
    fn parse_rust_tree(text: &str) -> tree_sitter::Tree {
        parse_with_language(text, tree_sitter_rust::LANGUAGE.into())
    }

    #[test]
    fn is_in_exclusion_range_empty_ranges_returns_false() {
        let tree = parse_rust_tree("fn main() {}");
        let root = tree.root_node();
        assert!(
            !is_in_exclusion_range(&root, &[]),
            "Empty exclusion ranges should never match"
        );
    }

    #[test]
    fn is_in_exclusion_range_exact_match_not_excluded() {
        let tree = parse_rust_tree("fn main() {}");
        let root = tree.root_node();
        // Root node spans [0, 12) — exactly matches the exclusion range.
        // Exact match should NOT be excluded (the sweep line splitter in
        // `finalize_tokens` resolves same-position conflicts).
        // This matches the Markdown heading case: @markup.heading.1 is captured on
        // the same node as the markdown_inline injection content.
        assert!(
            !is_in_exclusion_range(&root, &[(0, 12)]),
            "Exact match should NOT be excluded"
        );
    }

    #[test]
    fn effective_prefix_widths_variable_width_block_continuation() {
        // In `> # foo\n>\n`, the atx_heading spans [0,2]-[1,1].
        // The block_continuation on line 1 is just `>` (no trailing space),
        // ending at col 1 — shorter than the heading's start_col of 2.
        // The prefix detection must still recognize this as a structural prefix
        // to prevent the heading token from leaking onto the empty `>` line.
        let text = "> # foo\n>\n> bar\n>\n";
        let tree = parse_with_language(text, tree_sitter_md::LANGUAGE.into());
        // Navigate: document > section > block_quote > section > atx_heading
        let root = tree.root_node();
        let block_quote = root.child(0).unwrap().child(0).unwrap();
        assert_eq!(block_quote.kind(), "block_quote");
        let section = block_quote.named_child(1).unwrap();
        assert_eq!(section.kind(), "section");
        let heading = section.named_child(0).unwrap();
        assert_eq!(heading.kind(), "atx_heading");
        assert_eq!(heading.start_position().column, 2);
        assert_eq!(heading.end_position().row, 1);

        let widths = effective_prefix_widths(&heading, &[]);
        // Row 1 must have a non-zero width to prevent the heading from leaking
        assert!(
            widths.len() > 1 && widths[1] > 0,
            "Variable-width block_continuation (`>` without space) must still be \
             detected as structural prefix. Got widths: {:?}",
            widths
        );
    }

    #[test]
    fn is_in_exclusion_range_strictly_contained() {
        // "fn" keyword node spans bytes [0, 2), which is strictly inside [0, 12)
        let tree = parse_rust_tree("fn main() {}");
        let fn_node = tree.root_node().child(0).unwrap().child(0).unwrap();
        assert_eq!((fn_node.start_byte(), fn_node.end_byte()), (0, 2));
        assert!(
            is_in_exclusion_range(&fn_node, &[(0, 12)]),
            "Node strictly inside range should be excluded"
        );
    }

    #[test]
    fn is_in_exclusion_range_partial_overlap_start_not_excluded() {
        let tree = parse_rust_tree("fn main() {}");
        let root = tree.root_node();
        // Root [0, 12), range [0, 3) — root extends beyond range → not contained
        assert!(
            !is_in_exclusion_range(&root, &[(0, 3)]),
            "Node extending beyond range should NOT be excluded"
        );
    }

    #[test]
    fn is_in_exclusion_range_partial_overlap_end_not_excluded() {
        let tree = parse_rust_tree("fn main() {}");
        let root = tree.root_node();
        // Root [0, 12), range [10, 15) — root starts before range → not contained
        assert!(
            !is_in_exclusion_range(&root, &[(10, 15)]),
            "Node starting before range should NOT be excluded"
        );
    }

    #[test]
    fn is_in_exclusion_range_no_overlap_before() {
        // "fn" keyword node spans bytes [0, 2)
        let tree = parse_rust_tree("fn main() {}");
        let fn_node = tree.root_node().child(0).unwrap().child(0).unwrap(); // fn keyword
        let start = fn_node.start_byte();
        let end = fn_node.end_byte();
        assert_eq!((start, end), (0, 2));
        // Range is entirely after the node
        assert!(!is_in_exclusion_range(&fn_node, &[(5, 10)]));
    }

    #[test]
    fn is_in_exclusion_range_no_overlap_after() {
        let tree = parse_rust_tree("fn main() {}");
        let fn_node = tree.root_node().child(0).unwrap().child(0).unwrap();
        // Range is entirely before the node (empty range at byte 0 doesn't overlap [0, 2))
        // Actually [0, 0) is empty so no overlap. Let's use a range that ends at node start.
        assert!(!is_in_exclusion_range(&fn_node, &[(10, 12)]));
    }

    #[test]
    fn is_in_exclusion_range_adjacent_not_overlapping() {
        // Node [0, 2), range [2, 5) — these are adjacent but NOT overlapping
        let tree = parse_rust_tree("fn main() {}");
        let fn_node = tree.root_node().child(0).unwrap().child(0).unwrap();
        assert_eq!(fn_node.end_byte(), 2);
        assert!(
            !is_in_exclusion_range(&fn_node, &[(2, 5)]),
            "Adjacent range should NOT overlap"
        );
    }

    #[test]
    fn is_in_exclusion_range_multiple_ranges_one_hits() {
        // "fn" keyword at [0, 2), check against multiple ranges
        let tree = parse_rust_tree("fn main() {}");
        let fn_node = tree.root_node().child(0).unwrap().child(0).unwrap();
        assert_eq!((fn_node.start_byte(), fn_node.end_byte()), (0, 2));
        // First range misses, second strictly contains [0, 2)
        assert!(is_in_exclusion_range(&fn_node, &[(100, 200), (0, 12)]));
    }

    // ── collect_host_tokens exclusion behavior ───────────────────────

    #[test]
    fn collect_host_tokens_exclusion_suppresses_strictly_contained_tokens() {
        // "fn main() {}" — "main" identifier node is at [3, 7)
        let code = "fn main() {}";
        let tree = parse_rust_tree(code);
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = tree_sitter::Query::new(&language, "(identifier) @variable").unwrap();
        let lines: Vec<&str> = code.lines().collect();

        // Without exclusion: should get the "main" identifier token
        let mut tokens_no_excl = Vec::new();
        collect_host_tokens(
            code,
            &tree,
            &query,
            Some("rust"),
            None,
            code,
            &lines,
            0,
            0,
            false,
            &[],
            &[],
            &mut tokens_no_excl,
        );
        assert!(
            !tokens_no_excl.is_empty(),
            "Without exclusion should produce tokens"
        );

        // Exclusion range [0, 12) strictly contains the identifier [3, 7) → suppressed
        let mut tokens_excl = Vec::new();
        collect_host_tokens(
            code,
            &tree,
            &query,
            Some("rust"),
            None,
            code,
            &lines,
            0,
            0,
            false,
            &[(0, code.len())],
            &[],
            &mut tokens_excl,
        );
        assert!(
            tokens_excl.is_empty(),
            "Identifier strictly inside exclusion range should be suppressed"
        );
    }

    #[test]
    fn collect_host_tokens_exclusion_exact_match_not_suppressed() {
        // "fn main() {}" — "main" identifier node is at [3, 7)
        let code = "fn main() {}";
        let tree = parse_rust_tree(code);
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = tree_sitter::Query::new(&language, "(identifier) @variable").unwrap();
        let lines: Vec<&str> = code.lines().collect();

        // Exclusion range [3, 7) exactly matches the identifier node → NOT suppressed.
        // This models the Markdown heading case where @markup.heading.1 is captured
        // on the same node that is the injection content.
        let mut tokens = Vec::new();
        collect_host_tokens(
            code,
            &tree,
            &query,
            Some("rust"),
            None,
            code,
            &lines,
            0,
            0,
            false,
            &[(3, 7)],
            &[],
            &mut tokens,
        );
        assert!(
            !tokens.is_empty(),
            "Token with exact-match exclusion range should NOT be suppressed"
        );
    }

    #[test]
    fn collect_host_tokens_exclusion_preserves_tokens_outside_range() {
        // "fn main() {}" — "fn" is at [0,2), "main" identifier at [3,7)
        let code = "fn main() {}";
        let tree = parse_rust_tree(code);
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        // Query that matches both "fn" keyword and "main" identifier
        let query = tree_sitter::Query::new(&language, r#"["fn"] @keyword (identifier) @variable"#)
            .unwrap();
        let lines: Vec<&str> = code.lines().collect();

        // Exclusion range [0, 12) strictly contains both "fn" [0,2) and "main" [3,7)
        let mut tokens = Vec::new();
        collect_host_tokens(
            code,
            &tree,
            &query,
            Some("rust"),
            None,
            code,
            &lines,
            0,
            0,
            false,
            &[(0, code.len())],
            &[],
            &mut tokens,
        );

        // Both are strictly contained → both suppressed
        assert!(
            tokens.is_empty(),
            "All tokens strictly inside exclusion should be suppressed"
        );

        // But with a range that only strictly contains "main" [3,7):
        // Use [2, 8) which contains [3,7) but not [0,2)
        let mut tokens2 = Vec::new();
        collect_host_tokens(
            code,
            &tree,
            &query,
            Some("rust"),
            None,
            code,
            &lines,
            0,
            0,
            false,
            &[(2, 8)],
            &[],
            &mut tokens2,
        );

        let has_keyword = tokens2
            .iter()
            .any(|t| t.kind == TokenKind::Mapped("keyword".to_string()));
        let has_variable = tokens2
            .iter()
            .any(|t| t.kind == TokenKind::Mapped("variable".to_string()));
        assert!(has_keyword, "fn keyword outside exclusion should be kept");
        assert!(
            !has_variable,
            "main identifier strictly inside exclusion should be dropped"
        );
    }

    // ── effective_prefix_widths tests ──────────────────────────────

    #[test]
    fn effective_prefix_widths_returns_empty_for_indented_multiline_rust_node() {
        // A multiline struct field list starts at col > 0 but has no structural
        // prefix children (no named child at col 0 ending at the node's start col).
        // This must return empty to avoid false-positive prefix trimming.
        let code = "struct S {\n    x: i32,\n    y: bool,\n}";
        let tree = parse_rust_tree(code);
        let root = tree.root_node();
        // The struct body `{ x: i32, y: bool, }` is a field_declaration_list
        // starting at col 9 on line 0, spanning multiple lines.
        let item = root.child(0).expect("struct item");
        let field_list = item
            .child_by_field_name("body")
            .expect("field_declaration_list");
        assert!(
            field_list.start_position().column > 0,
            "field_declaration_list should start at col > 0"
        );
        assert!(
            field_list.end_position().row > field_list.start_position().row,
            "field_declaration_list should span multiple lines"
        );

        let widths = effective_prefix_widths(&field_list, &[]);
        assert!(
            widths.is_empty(),
            "Indented multiline Rust node with no structural prefix children \
             should return empty prefix widths, got: {:?}",
            widths
        );
    }

    #[test]
    fn effective_prefix_widths_passthrough_when_already_provided() {
        // When prefix_byte_widths is non-empty (injection-level) and the node
        // has no inner structural prefix children, return as-is.
        let code = "fn main() {}";
        let tree = parse_rust_tree(code);
        let root = tree.root_node();
        let provided = vec![0, 2, 2];
        let widths = effective_prefix_widths(&root, &provided);
        assert_eq!(widths, provided);
    }

    #[test]
    fn effective_prefix_widths_combines_outer_and_inner_prefix() {
        // Simulate injection: markdown parsed with included_ranges that exclude
        // the outer ">> " prefix. The inner "> " prefix inside the blockquote
        // must be detected and combined with the outer prefix widths.
        //
        // Content: ">> > # foo\n>> > bar\n"
        // Included ranges skip ">> " (bytes 0-2) on each line.
        // The injection parser sees "> # foo\n" and "> bar\n".
        // The heading/section node starts at col 5 (after ">> > ").
        // The block_continuation "> " at (1, 3)-(1, 5) is an inner prefix.
        let mut parser = tree_sitter::Parser::new();
        let language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        parser.set_language(&language).unwrap();

        let text = ">> > # foo\n>> > bar\n";
        parser
            .set_included_ranges(&[
                tree_sitter::Range {
                    start_byte: 3,
                    end_byte: 11,
                    start_point: tree_sitter::Point { row: 0, column: 3 },
                    end_point: tree_sitter::Point { row: 1, column: 0 },
                },
                tree_sitter::Range {
                    start_byte: 14,
                    end_byte: 20,
                    start_point: tree_sitter::Point { row: 1, column: 3 },
                    end_point: tree_sitter::Point { row: 2, column: 0 },
                },
            ])
            .unwrap();

        let tree = parser.parse(text, None).unwrap();
        let root = tree.root_node();

        // Navigate to the section or heading node that spans multiple lines.
        // Tree structure: document > (section >) block_quote > section > atx_heading
        // Find the multiline node with start_col > 0.
        let mut target_node = None;
        let mut cursor = root.walk();
        // Find the atx_heading node (which contains block_continuation as a child)
        fn find_atx_heading<'a>(
            cursor: &mut tree_sitter::TreeCursor<'a>,
            target: &mut Option<tree_sitter::Node<'a>>,
        ) {
            let node = cursor.node();
            if node.kind() == "atx_heading" {
                *target = Some(node);
                return;
            }
            if cursor.goto_first_child() {
                loop {
                    find_atx_heading(cursor, target);
                    if target.is_some() {
                        return;
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
                cursor.goto_parent();
            }
        }
        find_atx_heading(&mut cursor, &mut target_node);
        let node = target_node.expect("Should find atx_heading node");

        // Outer prefix widths: ">> " = 3 bytes on each line
        let outer_prefix_widths = vec![3, 3];
        let widths = effective_prefix_widths(&node, &outer_prefix_widths);

        // Row 1 should have width >= 5 (3 for ">> " outer + 2 for "> " inner)
        // because the block_continuation "> " at (1, 3)-(1, 5) is a structural
        // prefix child that extends the effective prefix beyond the outer width.
        assert!(
            widths.len() > 1 && widths[1] >= 5,
            "Row 1 effective prefix should be >= 5 (outer 3 + inner 2). \
             Got widths: {:?}. The inner '> ' prefix must be detected even \
             when outer prefix_byte_widths is provided.",
            widths
        );
    }

    #[test]
    fn effective_prefix_widths_returns_empty_for_col_zero_node() {
        // A node starting at column 0 has no prefix to strip.
        let code = "struct S {\n    x: i32,\n}";
        let tree = parse_rust_tree(code);
        let root = tree.root_node();
        assert_eq!(root.start_position().column, 0);
        let widths = effective_prefix_widths(&root, &[]);
        assert!(widths.is_empty());
    }
}
