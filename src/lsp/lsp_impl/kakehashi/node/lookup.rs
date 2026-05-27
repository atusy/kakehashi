//! Shared helpers for resolving tracker entries back to tree-sitter nodes.
//!
//! Multiple ADR-0025 handlers (`parent`, `children`, …) need to recover a
//! `tree_sitter::Node` from a tracked `(start_byte, end_byte, kind)` triple in
//! the current parse tree. Tree-sitter exposes no direct "find by composite
//! key" API, so this module centralises the upward-walk strategy used by all
//! navigation handlers — keeping the lookup semantics consistent and avoiding
//! drift across copies.

/// Find a tree-sitter node in `tree` whose `(start_byte, end_byte, kind)`
/// matches the tracked triple.
///
/// Starts from the smallest descendant covering `[start, end)` (which is the
/// usual case for a tracked node) and walks up to the root looking for an
/// exact match. The walk is bounded by tree depth and runs in `O(depth)` time.
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
    // `descendant_for_byte_range(start, end)` returns the smallest node whose
    // byte range contains `[start, end)`. For a tracked node, that's the node
    // itself; for stale ranges it returns the containing node, in which case
    // the upward walk below will fail to find a match (and we return None).
    let mut current = tree.root_node().descendant_for_byte_range(start, end)?;

    loop {
        if current.start_byte() == start && current.end_byte() == end && current.kind() == kind {
            return Some(current);
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return None,
        }
    }
}
