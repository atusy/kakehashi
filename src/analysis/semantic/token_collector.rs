//! Token collection from tree-sitter queries.
//!
//! This module handles the collection of raw tokens from a single document's
//! highlight query, including multiline token handling and byte-to-UTF16 conversion.

use crate::config::CaptureMappings;
use crate::text::convert_byte_to_utf16_in_line;
use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

use super::legend::apply_capture_mapping;

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

/// Represents a token before delta encoding with all position information.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawToken {
    /// 0-indexed line number in the host document
    pub line: usize,
    /// UTF-16 column position within the line
    pub column: usize,
    /// Length in UTF-16 code units
    pub length: usize,
    /// Mapped capture name (e.g., "keyword", "variable.readonly")
    pub mapped_name: String,
    /// Injection depth (0 = host document)
    pub depth: usize,
    /// Index of the query pattern that produced this token.
    /// Within a single query, later patterns (higher index) are more specific
    /// and should override earlier ones at the same position and depth.
    pub pattern_index: usize,
}

/// Represents the line/column boundaries of an injection region in the host document.
///
/// Used to exclude host tokens that fall inside active injection regions during finalization.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InjectionRegion {
    /// Start line (0-indexed)
    pub start_line: usize,
    /// Start column (UTF-16)
    pub start_col: usize,
    /// End line (0-indexed)
    pub end_line: usize,
    /// End column (UTF-16)
    pub end_col: usize,
}

/// Convert byte column position to UTF-16 column position within a line
/// This is a wrapper around the common utility for backward compatibility
pub(super) fn byte_to_utf16_col(line: &str, byte_col: usize) -> usize {
    // The common utility returns Option, but we need to handle the case where
    // byte_col is beyond the end of the line or in the middle of a character
    convert_byte_to_utf16_in_line(line, byte_col).unwrap_or_else(|| {
        // If conversion fails (e.g., byte_col is in the middle of a multi-byte char),
        // find the nearest valid position
        let mut valid_col = byte_col;
        while valid_col > 0 {
            if let Some(utf16) = convert_byte_to_utf16_in_line(line, valid_col) {
                return utf16;
            }
            valid_col -= 1;
        }
        // Fallback to 0 if no valid position found
        0
    })
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
) -> (usize, usize) {
    // Calculate start byte offset for this line
    let line_start = if row == start_pos.row {
        if row == 0 {
            content_start_col + start_pos.column
        } else {
            start_pos.column
        }
    } else {
        // Continuation lines start at column 0
        0
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
        let filtered_captures = crate::language::filter_captures(query, m, text);

        for c in filtered_captures {
            let node = c.node;
            let start_pos = node.start_position();
            let end_pos = node.end_position();

            // Check if this is a single-line token or trailing newline case
            let is_single_line = start_pos.row == end_pos.row;
            let is_trailing_newline = end_pos.row == start_pos.row + 1 && end_pos.column == 0;

            // Get the mapped capture name early to avoid repeated mapping
            let capture_name = &query.capture_names()[c.index as usize];
            let Some(mapped_name) = apply_capture_mapping(capture_name, filetype, capture_mappings)
            else {
                // Skip unknown captures (None)
                continue;
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
                    mapped_name,
                    depth,
                    pattern_index: m.pattern_index,
                });
            } else if supports_multiline {
                // Multiline token with client support: emit a single token spanning multiple lines.
                // LSP semantic tokens use line-relative positions, so the token naturally starts on
                // the first line (start_pos.row), and its length spans across all lines in UTF-16
                // code units (including newline characters) up to the end position on end_pos.row.
                //
                // The length is calculated by summing UTF-16 lengths across all lines of the token,
                // plus 1 for each newline character between lines.
                let host_start_line = content_start_line + start_pos.row;
                let host_end_line = content_start_line + end_pos.row;

                // Calculate start position
                let host_start_line_text = host_lines.get(host_start_line).unwrap_or(&"");
                let start_byte_offset = if start_pos.row == 0 {
                    content_start_col + start_pos.column
                } else {
                    start_pos.column
                };
                let start_utf16 = byte_to_utf16_col(host_start_line_text, start_byte_offset);

                // Calculate total length in UTF-16 code units across all lines
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
                    );

                    let line_start_utf16 = byte_to_utf16_col(line_text, line_start);
                    let line_end_utf16 = byte_to_utf16_col(line_text, line_end);
                    total_length_utf16 += line_end_utf16 - line_start_utf16;

                    // Add 1 for newline character between lines (except last line)
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
                    mapped_name,
                    depth,
                    pattern_index: m.pattern_index,
                });
            } else {
                // Multiline token without client support: split into per-line tokens
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
                    );

                    let start_utf16 = byte_to_utf16_col(host_line_text, line_start_byte);
                    let end_utf16 = byte_to_utf16_col(host_line_text, line_end_byte);

                    // Skip empty tokens
                    if end_utf16 > start_utf16 {
                        all_tokens.push(RawToken {
                            line: host_row,
                            column: start_utf16,
                            length: end_utf16 - start_utf16,
                            mapped_name: mapped_name.clone(),
                            depth,
                            pattern_index: m.pattern_index,
                        });
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_in_exclusion_range tests ──────────────────────────────────

    /// Helper: parse `text` with the given language and return the root node's
    /// first child (or root itself) for exclusion-range testing.
    fn parse_rust_tree(text: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        parser.parse(text, None).unwrap()
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
            &mut tokens2,
        );

        let has_keyword = tokens2.iter().any(|t| t.mapped_name == "keyword");
        let has_variable = tokens2.iter().any(|t| t.mapped_name == "variable");
        assert!(has_keyword, "fn keyword outside exclusion should be kept");
        assert!(
            !has_variable,
            "main identifier strictly inside exclusion should be dropped"
        );
    }

    #[test]
    fn byte_to_utf16_col_ascii() {
        let line = "hello world";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 5), 5);
        assert_eq!(byte_to_utf16_col(line, 11), 11);
    }

    #[test]
    fn byte_to_utf16_col_japanese() {
        // Japanese text (3 bytes per char in UTF-8, 1 code unit in UTF-16)
        let line = "こんにちは";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 3), 1); // After "こ"
        assert_eq!(byte_to_utf16_col(line, 6), 2); // After "こん"
        assert_eq!(byte_to_utf16_col(line, 15), 5); // After all 5 chars
    }

    #[test]
    fn byte_to_utf16_col_mixed_ascii_and_japanese() {
        let line = "let x = \"あいうえお\"";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 8), 8); // Before '"'
        assert_eq!(byte_to_utf16_col(line, 9), 9); // Before "あ"
        assert_eq!(byte_to_utf16_col(line, 12), 10); // After "あ" (3 bytes -> 1 UTF-16)
        assert_eq!(byte_to_utf16_col(line, 24), 14); // After "あいうえお\"" (15 bytes + 1 quote)
    }

    #[test]
    fn byte_to_utf16_col_emoji() {
        // Emoji (4 bytes in UTF-8, 2 code units in UTF-16)
        let line = "hello 👋 world";
        assert_eq!(byte_to_utf16_col(line, 0), 0);
        assert_eq!(byte_to_utf16_col(line, 6), 6); // After "hello "
        assert_eq!(byte_to_utf16_col(line, 10), 8); // After emoji (4 bytes -> 2 UTF-16)
    }
}
