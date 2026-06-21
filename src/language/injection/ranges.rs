//! Computing the included byte ranges handed to the injection parser: gap
//! ranges between a content node's named children, window clipping for
//! `#offset!`, nested sub-selection, and intersection.

use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};

use super::byte_to_point_anchored;

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
