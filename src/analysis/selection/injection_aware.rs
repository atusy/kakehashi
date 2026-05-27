//! Injection-aware SelectionRange building utilities.
//!
//! This module contains functions that handle language injection scenarios,
//! where one language is embedded within another (e.g., YAML in Markdown frontmatter).
//! These functions manage coordinate translation between injection and host documents,
//! and build proper selection hierarchies that respect injection boundaries.

use tower_lsp_server::ls_types::{Position, Range, SelectionRange};
use tree_sitter::Node;

use super::range_builder::node_to_range;
use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
use crate::language::injection::InjectionOffset;
use crate::text::position::PositionMapper;

/// Adjust a node's range from injection-relative to host-document-relative coordinates.
///
/// When working with injected language content, Tree-sitter parses the injection
/// as a standalone document starting at byte 0. This function translates those
/// byte positions back to the host document's coordinate space.
///
/// Uses byte offsets and PositionMapper to ensure correct UTF-16 column conversion.
///
/// # Arguments
/// * `node` - The Tree-sitter node from the injected language parse tree
/// * `content_start_byte` - The byte offset where the injection content starts in the host document
/// * `mapper` - PositionMapper for the host document (byte-to-UTF16 conversion)
///
/// # Returns
/// LSP Range in host document coordinates with proper UTF-16 positions
pub fn adjust_range_to_host(
    node: Node,
    content_start_byte: usize,
    mapper: &PositionMapper,
) -> Range {
    // Calculate the actual byte offsets in the host document
    // Node's start_byte() and end_byte() are relative to the injection content
    let host_start_byte = content_start_byte + node.start_byte();
    let host_end_byte = content_start_byte + node.end_byte();

    // Convert byte offsets to UTF-16 positions using the mapper
    let adjusted_start = mapper
        .byte_to_position(host_start_byte)
        .unwrap_or_else(|| Position::new(0, 0));
    let adjusted_end = mapper
        .byte_to_position(host_end_byte)
        .unwrap_or_else(|| Position::new(0, 0));

    Range::new(adjusted_start, adjusted_end)
}

/// Calculate the effective LSP Range after applying offset to content node.
///
/// Offset directives (like `#offset! @injection.content 1 0 -1 0`) adjust where
/// the injection content actually starts and ends. This function applies those
/// offsets and converts the result to an LSP Range with proper UTF-16 positions.
///
/// # Arguments
/// * `text` - The full host document text
/// * `mapper` - PositionMapper for byte-to-UTF16 conversion
/// * `content_node` - The injection content node
/// * `offset` - The offset to apply (row and column adjustments)
///
/// # Returns
/// LSP Range representing the effective injection boundaries
pub fn calculate_effective_lsp_range(
    text: &str,
    mapper: &PositionMapper,
    content_node: &Node,
    offset: InjectionOffset,
) -> Range {
    let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
    let effective = calculate_effective_range(text, byte_range, offset);

    // Convert byte positions to LSP positions (reusing cached mapper)
    let start_pos = mapper
        .byte_to_position(effective.start)
        .unwrap_or_else(|| Position::new(0, 0));
    let end_pos = mapper
        .byte_to_position(effective.end)
        .unwrap_or_else(|| Position::new(0, 0));

    Range::new(start_pos, end_pos)
}

/// True iff `cursor_byte` lies inside the injection content after applying
/// `offset` directives. Example: Markdown frontmatter with `(1, 0, -1, 0)`
/// excludes the `---` boundary lines, so only positions inside `title: "hello"\n`
/// qualify.
pub fn is_cursor_within_effective_range(
    text: &str,
    content_node: &Node,
    cursor_byte: usize,
    offset: InjectionOffset,
) -> bool {
    let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
    let effective_range = calculate_effective_range(text, byte_range, offset);
    cursor_byte >= effective_range.start && cursor_byte < effective_range.end
}

/// Check if a node's range is already present in the selection chain.
///
/// Used to determine if we need to splice the injection content node into
/// the hierarchy, or if it's already there from a previous traversal.
///
/// # Arguments
/// * `selection` - The root of the SelectionRange chain to search
/// * `target_node` - The node whose range we're looking for
/// * `mapper` - PositionMapper for converting node to LSP Range
///
/// # Returns
/// `true` if the node's range appears somewhere in the chain, `false` otherwise
pub fn is_node_in_selection_chain(
    selection: &SelectionRange,
    target_node: &Node,
    mapper: &PositionMapper,
) -> bool {
    let target_range = node_to_range(*target_node, mapper);
    let mut current = Some(selection);

    while let Some(sel) = current {
        if sel.range == target_range {
            return true;
        }
        current = sel.parent.as_ref().map(|p| p.as_ref());
    }

    false
}

// Note: Unit tests for injection_aware functions require Tree-sitter parsers
// and injection scenarios which are tested through integration tests in selection.rs.
// The tests in selection.rs cover:
// - test_selection_range_handles_nested_injection
// - test_injected_selection_range_uses_utf16_columns
// - test_nested_injection_includes_content_node_boundary
// - test_selection_range_respects_offset_directive (calculate_effective_lsp_range, is_cursor_within_effective_range)
// - is_node_in_selection_chain is tested implicitly via the injection-aware selection tests
