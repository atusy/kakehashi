//! Computing the included byte ranges handed to the injection parser: gap
//! ranges between a content node's named children, window clipping for
//! `#offset!`, nested sub-selection, and intersection.

use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};

use super::content::byte_to_point_anchored;

/// Checks whether the given pattern has `#set! injection.combined`.
///
/// Like `injection.include-children`, this is a value-less property — its
/// **presence** means all `@injection.content` captures of the pattern (for
/// the same language) are parsed as one document via `set_included_ranges`,
/// so cross-region context survives (e.g. an HTML tag opened in one
/// `html_block` and closed in another). See #187.
pub(crate) fn has_combined_for_pattern(query: &tree_sitter::Query, pattern_index: usize) -> bool {
    for predicate in get_all_predicates(query, pattern_index) {
        if let UnifiedPredicate::Property(prop) = predicate
            && prop.key.as_ref() == "injection.combined"
        {
            return true;
        }
    }
    false
}

/// Checks whether the given pattern has `#set! injection.include-children`.
///
/// This is a value-less property — its **presence** alone means "include children"
/// (the injection parser sees the full content node including named children).
/// When absent, named children should be excluded via `set_included_ranges()`.
pub(crate) fn has_include_children_for_pattern(
    query: &tree_sitter::Query,
    pattern_index: usize,
) -> bool {
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
    compute_included_ranges_in_window(
        content_node,
        include_children,
        content_node.start_byte(),
        content_node.end_byte(),
        || (content_node.start_position(), content_node.end_position()),
    )
}

/// Like [`compute_included_ranges`], but restricted to an effective byte
/// window — the post-`#offset!` span (#186). Children outside the window are
/// ignored; children straddling a window edge are clipped to it. Returned
/// ranges are relative to `window.start`, matching the content slice handed
/// to the injection parser.
///
/// `window` bounds must lie on char boundaries (callers snap with
/// `ceil_char_boundary` / `floor_char_boundary` before slicing the content).
pub(crate) fn compute_included_ranges_clipped(
    content_node: &tree_sitter::Node,
    include_children: bool,
    text: &str,
    window: std::ops::Range<usize>,
) -> Option<Vec<tree_sitter::Range>> {
    if window.start >= window.end {
        return None;
    }
    compute_included_ranges_in_window(
        content_node,
        include_children,
        window.start,
        window.end,
        || {
            // Anchor the scans on the content node's cached position so we only
            // walk the (typically small) span between the node start and the
            // window edges, not the whole document prefix. Lazy: skipped entirely
            // when the guards in the core return early.
            let anchor_byte = content_node.start_byte();
            let anchor_point = content_node.start_position();
            let start = byte_to_point_anchored(text, window.start, anchor_byte, anchor_point);
            let end = byte_to_point_anchored(text, window.end, window.start, start);
            (start, end)
        },
    )
}

/// Shared core of [`compute_included_ranges`] /
/// [`compute_included_ranges_clipped`]: gaps between the content node's named
/// children, restricted to `[window_start_byte, window_end_byte)` and
/// relativized to the window start. `window_points` lazily supplies the
/// window bounds' Points — it is only invoked once the guards have decided
/// gaps actually need computing.
fn compute_included_ranges_in_window(
    content_node: &tree_sitter::Node,
    include_children: bool,
    window_start_byte: usize,
    window_end_byte: usize,
    window_points: impl FnOnce() -> (tree_sitter::Point, tree_sitter::Point),
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

    let (window_start_point, window_end_point) = window_points();

    // Helper to make a Point relative to the window's start.
    // Column is only adjusted when the point is on the same row as the window
    // start, because only that row has a non-zero column offset from the
    // window origin.
    let relativize_point = |p: tree_sitter::Point| -> tree_sitter::Point {
        tree_sitter::Point {
            row: p.row.saturating_sub(window_start_point.row),
            column: if p.row == window_start_point.row {
                p.column.saturating_sub(window_start_point.column)
            } else {
                p.column
            },
        }
    };

    let mut ranges = Vec::new();
    let mut cursor_byte = window_start_byte;
    let mut cursor_point = window_start_point;

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

        // Entirely before the window (or before an earlier child that already
        // advanced the cursor): nothing to exclude here.
        if child_end_byte <= cursor_byte {
            continue;
        }
        // Children are in document order; the first one at/after the window
        // end means no later child can affect the window either.
        if child_start_byte >= window_end_byte {
            break;
        }

        // Gap before this child
        if cursor_byte < child_start_byte {
            ranges.push(tree_sitter::Range {
                start_byte: cursor_byte - window_start_byte,
                end_byte: child_start_byte - window_start_byte,
                start_point: relativize_point(cursor_point),
                end_point: relativize_point(child.start_position()),
            });
        }

        // A child running past the window end is clipped: the window's tail
        // is covered, so no final gap remains.
        if child_end_byte >= window_end_byte {
            cursor_byte = window_end_byte;
            cursor_point = window_end_point;
            break;
        }

        cursor_byte = child_end_byte;
        cursor_point = child.end_position();
    }

    // Gap after last child
    if cursor_byte < window_end_byte {
        ranges.push(tree_sitter::Range {
            start_byte: cursor_byte - window_start_byte,
            end_byte: window_end_byte - window_start_byte,
            start_point: relativize_point(cursor_point),
            end_point: relativize_point(window_end_point),
        });
    }

    if ranges.is_empty() {
        None
    } else {
        Some(ranges)
    }
}

/// Clip the parent's `included_ranges` to the nested injection's byte window
/// and re-relativize. Used when the nested injection's tree was parsed with
/// `included_ranges` and has no `block_continuation` children of its own;
/// both byte arguments are relative to `parent_ranges`' base (parent
/// `content_text` start). Returns `None` if no parent range overlaps.
///
/// Known limitation: when a range is clipped by `max`/`min`, only its bytes
/// are adjusted — `start_point`/`end_point` are not. Tree-sitter accepts this
/// (it validates byte ordering only), but nodes from clipped ranges may
/// report slightly wrong columns. Rare in practice: nested boundaries
/// usually align with parent gap boundaries.
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
            start_byte: clip_start.saturating_sub(nested_start_byte),
            end_byte: clip_end.saturating_sub(nested_start_byte),
            start_point: tree_sitter::Point {
                row: r.start_point.row.saturating_sub(base_row),
                column: start_column,
            },
            end_point: tree_sitter::Point {
                row: r.end_point.row.saturating_sub(base_row),
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

/// Intersect two sets of included ranges, keeping only byte regions present in both.
///
/// Both range sets must be in the same coordinate space (e.g., both relative
/// to `content_node.start_byte()`). The result contains only byte regions
/// that appear in BOTH inputs.
///
/// This is needed when a tree parsed with parent `included_ranges` produces
/// `compute_included_ranges()` gap ranges that inadvertently span parent-excluded
/// bytes. Intersecting with the parent's `sub_select_included_ranges()` output
/// ensures previously-excluded bytes stay excluded.
pub(crate) fn intersect_included_ranges(
    a: &[tree_sitter::Range],
    b: &[tree_sitter::Range],
) -> Vec<tree_sitter::Range> {
    let mut result = Vec::new();
    let mut j = 0;

    for ar in a {
        // Advance b past ranges that end before ar starts
        while j < b.len() && b[j].end_byte <= ar.start_byte {
            j += 1;
        }

        // Check all b ranges that overlap with ar
        let mut k = j;
        while k < b.len() && b[k].start_byte < ar.end_byte {
            let start_byte = ar.start_byte.max(b[k].start_byte);
            let end_byte = ar.end_byte.min(b[k].end_byte);

            if start_byte < end_byte {
                // Use point from whichever boundary is more restrictive
                let start_point = if start_byte == ar.start_byte {
                    ar.start_point
                } else {
                    b[k].start_point
                };
                let end_point = if end_byte == ar.end_byte {
                    ar.end_point
                } else {
                    b[k].end_point
                };

                result.push(tree_sitter::Range {
                    start_byte,
                    end_byte,
                    start_point,
                    end_point,
                });
            }
            k += 1;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::{Parser, Query};

    fn create_rust_parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        parser
    }

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
    fn test_has_combined_for_pattern() {
        // Pattern 0 has no combined property; pattern 1 mirrors the vendored
        // markdown html_block pattern shape.
        let query_str = r#"
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ((line_comment) @injection.content
              (#set! injection.language "html")
              (#set! injection.combined)
              (#set! injection.include-children))
        "#;
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        assert!(
            !has_combined_for_pattern(&query, 0),
            "Pattern without #set! injection.combined should return false"
        );
        assert!(
            has_combined_for_pattern(&query, 1),
            "Pattern with #set! injection.combined should return true"
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
    fn test_compute_included_ranges_clipped_full_window_matches_unclipped() {
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        let unclipped = compute_included_ranges(&func, false).expect("named children produce gaps");
        let clipped =
            compute_included_ranges_clipped(&func, false, text, func.start_byte()..func.end_byte())
                .expect("full window should produce the same gaps");

        assert_eq!(
            clipped, unclipped,
            "window covering the whole node must reproduce compute_included_ranges"
        );
    }

    #[test]
    fn test_compute_included_ranges_clipped_drops_children_before_window() {
        // "fn main() {}": named children of function_item are
        // main(3..7), parameters(7..9), body(10..12); gaps are [0..3) and [9..10).
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        // Window starts inside `main` (3..7): the straddling child is clipped,
        // the "fn " gap disappears, and only the " " gap [9..10) remains,
        // relative to the window start.
        let ranges = compute_included_ranges_clipped(&func, false, text, 4..12)
            .expect("gap inside window should survive");

        assert_eq!(ranges.len(), 1);
        assert_eq!((ranges[0].start_byte, ranges[0].end_byte), (5, 6));
        assert_eq!(
            ranges[0].start_point,
            tree_sitter::Point { row: 0, column: 5 }
        );
        assert_eq!(
            ranges[0].end_point,
            tree_sitter::Point { row: 0, column: 6 }
        );
    }

    #[test]
    fn test_compute_included_ranges_clipped_truncates_at_window_end() {
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        // Window ends inside the body (10..12): both gaps survive, and no
        // spurious gap is emitted after the clipped body.
        let ranges = compute_included_ranges_clipped(&func, false, text, 0..11)
            .expect("gaps inside window should survive");

        let bytes: Vec<(usize, usize)> =
            ranges.iter().map(|r| (r.start_byte, r.end_byte)).collect();
        assert_eq!(bytes, vec![(0, 3), (9, 10)]);
    }

    #[test]
    fn test_compute_included_ranges_clipped_returns_none_when_children_cover_window() {
        let mut parser = create_rust_parser();
        let text = "fn main() {}";
        let tree = parser.parse(text, None).unwrap();
        let func = tree.root_node().named_child(0).expect("function_item");

        // Window [10..12) lies entirely inside the body child: no gaps.
        assert!(
            compute_included_ranges_clipped(&func, false, text, 10..12).is_none(),
            "window fully covered by a child has no gaps"
        );
    }

    #[test]
    fn test_compute_included_ranges_clipped_relativizes_points_to_window_start() {
        // Two statements on separate lines; gaps are the newlines [4..5) and [9..10).
        let mut parser = create_rust_parser();
        let text = "a();\nb();\n";
        let tree = parser.parse(text, None).unwrap();
        let root = tree.root_node();

        // Window starts at byte 6 = row 1, column 1 (inside `b();`).
        let ranges = compute_included_ranges_clipped(&root, false, text, 6..10)
            .expect("trailing newline gap should survive");

        assert_eq!(ranges.len(), 1);
        // Gap [9..10) relative to window start 6.
        assert_eq!((ranges[0].start_byte, ranges[0].end_byte), (3, 4));
        // Same row as window start: column is relative (4 - 1 = 3).
        assert_eq!(
            ranges[0].start_point,
            tree_sitter::Point { row: 0, column: 3 }
        );
        // Next row: column stays absolute.
        assert_eq!(
            ranges[0].end_point,
            tree_sitter::Point { row: 1, column: 0 }
        );
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

    #[test]
    fn test_intersect_included_ranges_identical() {
        let ranges = vec![make_range(0, 4, 0, 0, 0, 4), make_range(6, 10, 1, 2, 1, 6)];
        let result = intersect_included_ranges(&ranges, &ranges);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_byte, 0);
        assert_eq!(result[0].end_byte, 4);
        assert_eq!(result[1].start_byte, 6);
        assert_eq!(result[1].end_byte, 10);
    }

    #[test]
    fn test_intersect_included_ranges_no_overlap() {
        let a = vec![make_range(0, 4, 0, 0, 0, 4)];
        let b = vec![make_range(6, 10, 1, 2, 1, 6)];
        let result = intersect_included_ranges(&a, &b);
        assert!(result.is_empty());
    }

    #[test]
    fn test_intersect_included_ranges_partial_overlap() {
        // a: [2..8], b: [5..12]
        // intersection: [5..8]
        let a = vec![make_range(2, 8, 0, 2, 0, 8)];
        let b = vec![make_range(5, 12, 0, 5, 0, 12)];
        let result = intersect_included_ranges(&a, &b);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_byte, 5);
        assert_eq!(result[0].end_byte, 8);
        // start_byte == b's start → use b's start_point
        assert_eq!(result[0].start_point.column, 5);
        // end_byte == a's end → use a's end_point
        assert_eq!(result[0].end_point.column, 8);
    }

    #[test]
    fn test_intersect_included_ranges_one_contains_other() {
        // a: [0..20], b: [5..10]
        // intersection: [5..10]
        let a = vec![make_range(0, 20, 0, 0, 0, 20)];
        let b = vec![make_range(5, 10, 0, 5, 0, 10)];
        let result = intersect_included_ranges(&a, &b);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_byte, 5);
        assert_eq!(result[0].end_byte, 10);
    }

    #[test]
    fn test_intersect_included_ranges_multiple_overlaps() {
        // a: [0..10, 20..30]
        // b: [5..25]
        // intersections: [5..10, 20..25]
        let a = vec![
            make_range(0, 10, 0, 0, 0, 10),
            make_range(20, 30, 2, 0, 2, 10),
        ];
        let b = vec![make_range(5, 25, 0, 5, 2, 5)];
        let result = intersect_included_ranges(&a, &b);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].start_byte, 5);
        assert_eq!(result[0].end_byte, 10);
        assert_eq!(result[1].start_byte, 20);
        assert_eq!(result[1].end_byte, 25);
    }

    #[test]
    fn test_intersect_included_ranges_empty_inputs() {
        let ranges = vec![make_range(0, 4, 0, 0, 0, 4)];
        assert!(intersect_included_ranges(&[], &ranges).is_empty());
        assert!(intersect_included_ranges(&ranges, &[]).is_empty());
        assert!(intersect_included_ranges(&[], &[]).is_empty());
    }

    #[test]
    fn test_intersect_included_ranges_adjacent_non_overlapping() {
        // a: [0..5], b: [5..10] — touching but not overlapping
        let a = vec![make_range(0, 5, 0, 0, 0, 5)];
        let b = vec![make_range(5, 10, 0, 5, 0, 10)];
        let result = intersect_included_ranges(&a, &b);
        assert!(result.is_empty(), "Adjacent ranges should not intersect");
    }
}
