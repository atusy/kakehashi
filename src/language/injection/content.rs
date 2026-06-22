//! Reusable content primitives shared by the semantic-tokens and
//! selection-range paths: clean-content extraction, per-line column offsets,
//! parsing with included ranges, and byte→`Point` coordinate conversion.

use crate::text::{clamped_slice, floor_char_boundary};

/// Extract clean injection content, stripping child node bytes when `included_ranges` present.
///
/// When `included_ranges` is `Some`, only the gap bytes (actual code content) are
/// concatenated, effectively stripping blockquote prefixes like `> `. When `None`,
/// returns the full content slice as-is.
pub(crate) fn extract_clean_content(
    host_text: &str,
    byte_range: std::ops::Range<usize>,
    included_ranges: Option<&[tree_sitter::Range]>,
) -> String {
    // `clamped_slice` guards against byte offsets from a stale tree that no
    // longer matches `host_text` (out of bounds or mid-codepoint).
    let content = clamped_slice(host_text, byte_range.clone());
    match included_ranges {
        None => content.to_string(),
        Some(ranges) => {
            let capacity: usize = ranges
                .iter()
                .map(|r| r.end_byte.saturating_sub(r.start_byte))
                .sum();
            let mut result = String::with_capacity(capacity);
            for range in ranges {
                result.push_str(clamped_slice(content, range.start_byte..range.end_byte));
            }
            result
        }
    }
}

/// Compute per-virtual-line UTF-16 column offsets from included ranges.
///
/// Each offset is the width of the blockquote prefix (or other excluded content)
/// before the gap on that line. When `included_ranges` is `None`, returns
/// `vec![start_column]` — the non-blockquote fallback where only line 0 has an offset.
pub(super) fn compute_line_column_offsets(
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
                            let line_start_byte =
                                gap_abs_byte.saturating_sub(range.start_point.column);
                            let prefix = clamped_slice(host_text, line_start_byte..gap_abs_byte);
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

/// Parse text with optional included ranges, resetting parser state afterward.
///
/// Shared protocol for Tree-sitter's `set_included_ranges` API; both semantic
/// tokens and selection range paths delegate here to avoid divergence. The reset
/// to `&[]` is unconditional (even on the failure path) so parsers returned to
/// pools or caches never carry stale included-range state.
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

/// Convert an absolute byte offset to a `tree_sitter::Point` (byte-based
/// column). Used when an offset directive shifts the injection boundary away
/// from a known node position, so we can't reuse the content node's
/// start/end points.
pub(crate) fn byte_to_point(text: &str, byte: usize) -> tree_sitter::Point {
    // Align first — slicing `&text[..clamped]` on a mid-character byte would
    // panic and crash the LSP server.
    let clamped = floor_char_boundary(text, byte);
    let prefix = &text[..clamped];
    let row = prefix.bytes().filter(|b| *b == b'\n').count();
    let last_nl = prefix.rfind('\n');
    let column = match last_nl {
        Some(idx) => clamped - idx - 1,
        None => clamped,
    };
    tree_sitter::Point { row, column }
}

/// Like [`byte_to_point`], but scans only the span between a known anchor
/// and `byte` instead of the whole prefix of `text` — the anchor is
/// typically a tree-sitter node's cached `start_position()`. Falls back to a
/// full scan when `byte` precedes the anchor (e.g. a negative `#offset!`
/// extending before the content node). `anchor_byte` must lie on a char
/// boundary, with `anchor_point` its Point in `text`.
pub(crate) fn byte_to_point_anchored(
    text: &str,
    byte: usize,
    anchor_byte: usize,
    anchor_point: tree_sitter::Point,
) -> tree_sitter::Point {
    let clamped = floor_char_boundary(text, byte);
    // Reuse the cached `anchor_point` only when `anchor_byte` is a real position
    // in the current text: if it's past the target, out of bounds, or
    // mid-codepoint (a stale tree), `anchor_point` no longer matches it and the
    // `text[anchor_byte..clamped]` slice could panic — fall back to a full scan,
    // which depends on neither.
    if clamped < anchor_byte || !text.is_char_boundary(anchor_byte) {
        return byte_to_point(text, clamped);
    }
    let segment = &text[anchor_byte..clamped];
    match segment.rfind('\n') {
        Some(last_nl) => tree_sitter::Point {
            row: anchor_point.row + segment.bytes().filter(|b| *b == b'\n').count(),
            column: segment.len() - last_nl - 1,
        },
        None => tree_sitter::Point {
            row: anchor_point.row,
            column: anchor_point.column + segment.len(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::injection::{collect_all_injections, compute_included_ranges};
    use tree_sitter::{Parser, Query, StreamingIterator};

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
    fn test_byte_to_point_anchored_matches_full_scan() {
        // Exhaustive equivalence with the full-prefix scan, covering the
        // before-anchor fallback, same-row and cross-row targets, and
        // mid-codepoint clamping (bytes inside あ floor to its start).
        let text = "abc\ndefあ\nghi";
        let anchor_byte = 4; // start of "def"
        let anchor_point = byte_to_point(text, anchor_byte);
        for byte in 0..=text.len() {
            assert_eq!(
                byte_to_point_anchored(text, byte, anchor_byte, anchor_point),
                byte_to_point(text, byte),
                "anchored and full scans must agree at byte {byte}"
            );
        }
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

    // --- stale-tree hardening: byte offsets that no longer match `text` must
    // degrade gracefully instead of panicking (#401). ---

    fn range_at(
        start_byte: usize,
        end_byte: usize,
        row: usize,
        column: usize,
    ) -> tree_sitter::Range {
        tree_sitter::Range {
            start_byte,
            end_byte,
            start_point: tree_sitter::Point { row, column },
            end_point: tree_sitter::Point { row, column },
        }
    }

    #[test]
    fn extract_clean_content_out_of_bounds_does_not_panic() {
        assert_eq!(extract_clean_content("hi", 0..100, None), "hi");
        assert_eq!(extract_clean_content("hi", 50..100, None), "");
        // Included sub-ranges past the end of the (clamped) content are skipped.
        let oversized = [range_at(0, 999, 0, 0)];
        assert_eq!(extract_clean_content("hi", 0..2, Some(&oversized)), "hi");
    }

    #[test]
    fn compute_line_column_offsets_oversized_column_does_not_panic() {
        // A non-first line whose start_point.column exceeds its absolute byte
        // position would underflow `gap_abs_byte - column` and slice past the end.
        let ranges = [range_at(2, 3, 1, 100)];
        let offsets = compute_line_column_offsets("hello", 0..5, 0, Some(&ranges));
        assert_eq!(offsets.len(), 2);
    }

    #[test]
    fn byte_to_point_anchored_stale_anchor_does_not_panic() {
        // "あ" is 3 bytes; an anchor landing mid-codepoint (byte 1) or past the
        // end must not panic the `text[anchor..clamped]` slice.
        let p = byte_to_point_anchored("あ", 3, 1, tree_sitter::Point { row: 0, column: 0 });
        assert_eq!(p.row, 0);
        let _ = byte_to_point_anchored("あ", 99, 50, tree_sitter::Point { row: 0, column: 0 });
    }
}
