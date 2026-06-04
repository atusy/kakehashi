//! Shared helpers for resolving tracker entries back to tree-sitter nodes.
//!
//! Multiple node-reference-protocol handlers (`parent`, `children`, …) need to recover a
//! `tree_sitter::Node` from a tracked `(start_byte, end_byte, kind)` triple in
//! the current parse tree. Tree-sitter exposes no direct "find by composite
//! key" API, so this module centralises the upward-walk strategy used by all
//! navigation handlers — keeping the lookup semantics consistent and avoiding
//! drift across copies.

/// Find a tree-sitter node in `tree` whose `(start_byte, end_byte, kind)`
/// matches the tracked triple.
///
/// `descendant_for_byte_range` returns the smallest node containing `[start,
/// end)`. If that node's range is not exactly `(start, end)`, no ancestor can
/// match either (ancestors only grow), so we bail immediately. When the range
/// matches but the kind doesn't, we walk upward but stop as soon as the
/// ancestor's range diverges from `(start, end)` — this handles the rare case
/// where multiple nodes share a span (e.g., a `document` wrapping a `section`
/// that wraps a `paragraph` all starting at byte 0) without doing useless work
/// on truly larger ancestors. The walk runs in O(depth-of-same-span chain).
///
/// Returns `None` when no node along that ancestry chain matches — typically a
/// sign that the tracker entry has drifted relative to the current tree (which
/// `didChange` should normally prevent).
pub(super) fn find_node_at<'tree>(
    tree: &'tree tree_sitter::Tree,
    start: usize,
    end: usize,
    kind: &str,
) -> Option<tree_sitter::Node<'tree>> {
    // Defensive: reject obviously-invalid ranges before handing them to
    // tree-sitter. A stale tracker entry (or future bug) could pass a range
    // outside the parsed tree's bounds; the underlying C bindings have, at
    // various tree-sitter versions, exhibited surprising behavior for such
    // inputs. Returning None preserves the caller's null-collapse semantics.
    let root = tree.root_node();
    if start > end || end > root.end_byte() {
        return None;
    }

    // `descendant_for_byte_range(start, end)` returns the smallest node whose
    // byte range contains `[start, end)`. If its range does not match exactly,
    // no node in the tree has `(start, end)` and we can stop here.
    let mut current = root.descendant_for_byte_range(start, end)?;
    if current.start_byte() != start || current.end_byte() != end {
        return None;
    }

    loop {
        if current.kind() == kind {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent.start_byte() == start && parent.end_byte() == end => {
                current = parent;
            }
            _ => return None,
        }
    }
}
