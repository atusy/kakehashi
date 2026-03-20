//! SelectionRange building from Tree-sitter AST nodes.
//!
//! This module provides the core functionality for building LSP SelectionRange
//! hierarchies from Tree-sitter AST nodes. It handles both simple cases (pure AST
//! traversal) and complex cases (language injections like YAML in Markdown).
//!
//! ## Entry Points
//!
//! - [`build`]: Main entry point. Detects injections and builds appropriate hierarchy.
//! - [`build_from_node`]: Pure AST traversal, no injection awareness.
//! - [`build_from_node_in_injection`]: For nodes already known to be in injected content.
//!
//! ## Architecture
//!
//! ```text
//! build()
//!   ├── No injection detected → build_from_node()
//!   └── Injection detected → parse injected content
//!       ├── Success → build_from_node_in_injection() + chain to host
//!       └── Failure → build_unparsed_fallback()
//! ```

use tower_lsp_server::ls_types::{Position, Range, SelectionRange};
use tree_sitter::{Node, Parser, Tree};

use super::context::{DocumentContext, InjectionContext};
use super::hierarchy_chain::{chain_injected_to_host, ranges_equal};
use super::injection_aware::{
    adjust_range_to_host, calculate_effective_lsp_range, is_cursor_within_effective_range,
    is_node_in_selection_chain,
};
use crate::analysis::offset_calculator::{ByteRange, calculate_effective_range};
use crate::language::injection::{
    self, compute_included_ranges, has_include_children_for_pattern,
    parse_offset_directive_for_pattern, parse_with_ranges,
};
use crate::text::PositionMapper;

/// Convert tree-sitter Node to LSP Range with proper UTF-16 encoding.
///
/// Tree-sitter stores positions as byte offsets, but LSP requires UTF-16 code units.
/// This function uses the provided PositionMapper to perform the correct conversion.
///
/// # Arguments
/// * `node` - The Tree-sitter node to convert
/// * `mapper` - PositionMapper for byte-to-UTF16 position conversion
///
/// # Returns
/// LSP Range with proper UTF-16 column positions
pub fn node_to_range(node: Node, mapper: &PositionMapper) -> Range {
    let start = mapper
        .byte_to_position(node.start_byte())
        .unwrap_or_else(|| Position::new(node.start_position().row as u32, 0));
    let end = mapper
        .byte_to_position(node.end_byte())
        .unwrap_or_else(|| Position::new(node.end_position().row as u32, 0));
    Range::new(start, end)
}

/// Find the next parent node that has a different (larger) byte range than the current node.
///
/// This ensures the LSP selection range hierarchy is strictly expanding.
/// The function walks up the AST tree until it finds a parent whose byte range
/// differs from the provided `current_range`.
///
/// # Arguments
/// * `node` - The starting node
/// * `current_range` - The byte range to compare against (typically the node's own range)
///
/// # Returns
/// The first ancestor with a different byte range, or None if no such ancestor exists
pub fn find_distinct_parent<'a>(
    node: Node<'a>,
    current_range: &std::ops::Range<usize>,
) -> Option<Node<'a>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        let parent_range = parent.byte_range();
        if parent_range != *current_range {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

/// Build a SelectionRange hierarchy by pure AST traversal.
///
/// Recursively constructs a chain of SelectionRange objects from the given node
/// up to the root, ensuring each parent range is strictly larger than its child.
/// Uses `find_distinct_parent` to skip nodes with identical ranges.
///
/// This function has no injection awareness - it simply walks up the AST.
/// For injection-aware building, use [`build`] instead.
///
/// # Arguments
/// * `node` - The starting Tree-sitter node
/// * `mapper` - PositionMapper for UTF-16 column conversion
///
/// # Returns
/// A SelectionRange with parent chain representing the AST hierarchy
pub fn build_from_node(node: Node, mapper: &PositionMapper) -> SelectionRange {
    let parent = find_distinct_parent(node, &node.byte_range())
        .map(|parent_node| Box::new(build_from_node(parent_node, mapper)));
    let range = node_to_range(node, mapper);
    SelectionRange { range, parent }
}

/// Build SelectionRange for a node that is inside injected content.
///
/// Similar to `build_from_node`, but adjusts all positions to be relative to
/// the host document (not the injection slice). Used when we've already parsed
/// the injected content and found a node within it.
///
/// # Arguments
/// * `node` - The Tree-sitter node from the injected language parse tree
/// * `content_start_byte` - Byte offset where injection content starts in host document
/// * `mapper` - PositionMapper for the host document
///
/// # Returns
/// SelectionRange hierarchy with positions adjusted to host document coordinates
pub fn build_from_node_in_injection(
    node: Node,
    content_start_byte: usize,
    mapper: &PositionMapper,
) -> SelectionRange {
    let parent = find_distinct_parent(node, &node.byte_range()).map(|parent_node| {
        Box::new(build_from_node_in_injection(
            parent_node,
            content_start_byte,
            mapper,
        ))
    });

    SelectionRange {
        range: adjust_range_to_host(node, content_start_byte, mapper),
        parent,
    }
}

/// Parse injection content with included-range exclusion for named children.
///
/// Computes included ranges to exclude named children (e.g., `block_continuation`
/// nodes in blockquote code blocks), then delegates to [`parse_with_ranges`] for
/// the set/parse/reset protocol. Skipped when an offset directive is active because
/// `compute_included_ranges()` returns ranges relative to `content_node.start_byte()`,
/// which would be misaligned with the offset-adjusted `content_text`.
///
/// Returns `None` if `set_included_ranges` fails (after logging a warning).
fn parse_with_included_ranges(
    parser: &mut Parser,
    content_text: &str,
    content_node: &Node,
    has_offset: bool,
    include_children: bool,
    lang_name: &str,
) -> Option<Tree> {
    // Skip included ranges when offset directive is active (misaligned byte ranges)
    let included_ranges = if has_offset {
        None
    } else {
        compute_included_ranges(content_node, include_children)
    };

    parse_with_ranges(
        parser,
        content_text,
        included_ranges.as_deref(),
        "kakehashi::selection",
        lang_name,
    )
}

/// Build SelectionRange with automatic injection detection and handling.
///
/// This is the main entry point for building selection ranges. It:
/// 1. Checks if the cursor is within an injection region
/// 2. If so, parses the injected content and builds a richer hierarchy
/// 3. Falls back to pure AST traversal when no injection is detected
///
/// # Arguments
/// * `node` - The node at cursor position in the host document
/// * `doc_ctx` - Document context (text, mapper, root, base_language)
/// * `inj_ctx` - Injection context (coordinator, parser_pool, depth tracking)
/// * `cursor_byte` - The byte offset of cursor position
///
/// # Returns
/// SelectionRange hierarchy, potentially spanning multiple language ASTs
pub fn build(
    node: Node,
    doc_ctx: &DocumentContext,
    inj_ctx: &mut InjectionContext,
    cursor_byte: usize,
) -> SelectionRange {
    let injection_query = inj_ctx.injection_query(doc_ctx.base_language);
    let injection_query_ref = injection_query.as_ref().map(|q| q.as_ref());

    let injection_info = injection::detect_injection(
        &node,
        &doc_ctx.root,
        doc_ctx.text,
        injection_query_ref,
        doc_ctx.base_language,
    );

    let Some((hierarchy, content_node, pattern_index)) = injection_info else {
        return build_from_node(node, doc_ctx.mapper);
    };

    if hierarchy.len() < 2 {
        return build_from_node(node, doc_ctx.mapper);
    }

    let offset_from_query =
        injection_query_ref.and_then(|q| parse_offset_directive_for_pattern(q, pattern_index));

    if let Some(offset) = offset_from_query
        && !is_cursor_within_effective_range(doc_ctx.text, &content_node, cursor_byte, offset)
    {
        return build_from_node(node, doc_ctx.mapper);
    }

    let injected_lang = &hierarchy[hierarchy.len() - 1];

    let build_fallback = || {
        let effective_range = offset_from_query.map(|offset| {
            calculate_effective_lsp_range(doc_ctx.text, doc_ctx.mapper, &content_node, offset)
        });
        build_unparsed_fallback(node, content_node, effective_range, doc_ctx.mapper)
    };

    if !inj_ctx.ensure_language_loaded(injected_lang) {
        return build_fallback();
    }

    let Some(mut parser) = inj_ctx.acquire_parser(injected_lang) else {
        return build_fallback();
    };

    let (content_text, effective_start_byte) = if let Some(offset) = offset_from_query {
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        let effective = calculate_effective_range(doc_ctx.text, byte_range, offset);
        (
            &doc_ctx.text[effective.start..effective.end],
            effective.start,
        )
    } else {
        (
            &doc_ctx.text[content_node.byte_range()],
            content_node.start_byte(),
        )
    };

    let include_children =
        injection_query_ref.is_some_and(|q| has_include_children_for_pattern(q, pattern_index));

    let Some(injected_tree) = parse_with_included_ranges(
        &mut parser,
        content_text,
        &content_node,
        offset_from_query.is_some(),
        include_children,
        injected_lang,
    ) else {
        inj_ctx.release_parser(injected_lang.to_string(), parser);
        return build_fallback();
    };

    let relative_byte = cursor_byte.saturating_sub(effective_start_byte);
    let injected_root = injected_tree.root_node();

    let Some(injected_node) = injected_root.descendant_for_byte_range(relative_byte, relative_byte)
    else {
        inj_ctx.release_parser(injected_lang.to_string(), parser);
        return build_fallback();
    };

    let nested_injection_query = inj_ctx.injection_query(injected_lang);

    let injected_selection = if let Some(nested_inj_query) = nested_injection_query.as_ref() {
        let nested_injection_info = injection::detect_injection(
            &injected_node,
            &injected_root,
            content_text,
            Some(nested_inj_query.as_ref()),
            injected_lang,
        );

        if let Some((nested_hierarchy, nested_content_node, nested_pattern_index)) =
            nested_injection_info
        {
            let nested_offset =
                parse_offset_directive_for_pattern(nested_inj_query.as_ref(), nested_pattern_index);

            let cursor_in_nested = match nested_offset {
                Some(offset) => is_cursor_within_effective_range(
                    content_text,
                    &nested_content_node,
                    relative_byte,
                    offset,
                ),
                None => true,
            };

            if cursor_in_nested && nested_hierarchy.len() >= 2 && inj_ctx.can_descend() {
                build_nested_injection(
                    &injected_node,
                    &injected_root,
                    content_text,
                    nested_inj_query.as_ref(),
                    injected_lang,
                    doc_ctx,
                    inj_ctx,
                    relative_byte,
                    effective_start_byte,
                )
            } else {
                build_from_node_in_injection(injected_node, effective_start_byte, doc_ctx.mapper)
            }
        } else {
            build_from_node_in_injection(injected_node, effective_start_byte, doc_ctx.mapper)
        }
    } else {
        build_from_node_in_injection(injected_node, effective_start_byte, doc_ctx.mapper)
    };

    let host_selection = Some(build_from_node(content_node, doc_ctx.mapper));
    let result = chain_injected_to_host(injected_selection, host_selection);
    inj_ctx.release_parser(injected_lang.to_string(), parser);
    result
}

/// Recursively build selection for deeply nested injections.
///
/// Handles cases like Markdown → YAML → embedded expression, where injections
/// are nested multiple levels deep. Uses `InjectionContext` for depth tracking.
#[allow(clippy::too_many_arguments)]
fn build_nested_injection(
    node: &Node,
    root: &Node,
    text: &str,
    injection_query: &tree_sitter::Query,
    base_language: &str,
    doc_ctx: &DocumentContext,
    inj_ctx: &mut InjectionContext,
    cursor_byte: usize,
    parent_start_byte: usize,
) -> SelectionRange {
    if inj_ctx.increment_depth().is_none() {
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    }

    let injection_info =
        injection::detect_injection(node, root, text, Some(injection_query), base_language);

    let Some((hierarchy, content_node, pattern_index)) = injection_info else {
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    };

    if hierarchy.len() < 2 {
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    }
    let nested_lang = hierarchy.last().unwrap().clone();

    if !inj_ctx.ensure_language_loaded(&nested_lang) {
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    }

    let offset = parse_offset_directive_for_pattern(injection_query, pattern_index);
    let (nested_text, nested_effective_start_byte) = if let Some(off) = offset {
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        let effective = calculate_effective_range(text, byte_range, off);
        (
            &text[effective.start..effective.end],
            parent_start_byte + effective.start,
        )
    } else {
        (
            &text[content_node.byte_range()],
            parent_start_byte + content_node.start_byte(),
        )
    };

    let Some(mut nested_parser) = inj_ctx.acquire_parser(&nested_lang) else {
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    };

    let include_children = has_include_children_for_pattern(injection_query, pattern_index);

    let Some(nested_tree) = parse_with_included_ranges(
        &mut nested_parser,
        nested_text,
        &content_node,
        offset.is_some(),
        include_children,
        &nested_lang,
    ) else {
        inj_ctx.release_parser(nested_lang.to_string(), nested_parser);
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    };

    let nested_relative_byte = if let Some(off) = offset {
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        let effective = calculate_effective_range(text, byte_range, off);
        cursor_byte.saturating_sub(effective.start)
    } else {
        cursor_byte.saturating_sub(content_node.start_byte())
    };

    let nested_root = nested_tree.root_node();

    let Some(nested_node) =
        nested_root.descendant_for_byte_range(nested_relative_byte, nested_relative_byte)
    else {
        inj_ctx.release_parser(nested_lang.to_string(), nested_parser);
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    };

    let deeply_nested_injection_query = inj_ctx.injection_query(&nested_lang);

    let nested_selection = if let Some(deep_inj_query) = deeply_nested_injection_query.as_ref()
        && inj_ctx.can_descend()
    {
        build_nested_injection(
            &nested_node,
            &nested_root,
            nested_text,
            deep_inj_query.as_ref(),
            &nested_lang,
            doc_ctx,
            inj_ctx,
            nested_relative_byte,
            nested_effective_start_byte,
        )
    } else {
        build_from_node_in_injection(nested_node, nested_effective_start_byte, doc_ctx.mapper)
    };

    let content_node_selection = Some(build_from_node_in_injection(
        content_node,
        parent_start_byte,
        doc_ctx.mapper,
    ));

    let result = chain_injected_to_host(nested_selection, content_node_selection);
    inj_ctx.release_parser(nested_lang.to_string(), nested_parser);
    result
}

/// Build selection when injection content cannot be parsed.
///
/// Used as fallback when the injection language is unavailable or parser fails.
/// Splices the effective_range into the host document's selection hierarchy.
fn build_unparsed_fallback(
    node: Node,
    content_node: Node,
    effective_range: Option<Range>,
    mapper: &PositionMapper,
) -> SelectionRange {
    let content_node_range = node_to_range(content_node, mapper);
    let inner_selection = build_from_node(node, mapper);

    if let Some(eff_range) = effective_range {
        if ranges_equal(&inner_selection.range, &content_node_range) {
            return SelectionRange {
                range: eff_range,
                parent: inner_selection
                    .parent
                    .map(|p| Box::new(replace_range_in_chain(*p, content_node_range, eff_range))),
            };
        }

        if is_node_in_selection_chain(&inner_selection, &content_node, mapper) {
            return replace_range_in_chain(inner_selection, content_node_range, eff_range);
        }
    } else if is_node_in_selection_chain(&inner_selection, &content_node, mapper) {
        return inner_selection;
    }

    splice_effective_range_into_hierarchy(
        inner_selection,
        effective_range.unwrap_or(content_node_range),
        &content_node,
        mapper,
    )
}

/// Replace a specific range in the selection chain with the effective range.
fn replace_range_in_chain(
    selection: SelectionRange,
    target_range: Range,
    effective_range: Range,
) -> SelectionRange {
    SelectionRange {
        range: if ranges_equal(&selection.range, &target_range) {
            effective_range
        } else {
            selection.range
        },
        parent: selection
            .parent
            .map(|p| Box::new(replace_range_in_chain(*p, target_range, effective_range))),
    }
}

fn splice_effective_range_into_hierarchy(
    selection: SelectionRange,
    effective_range: Range,
    content_node: &Node,
    mapper: &PositionMapper,
) -> SelectionRange {
    use super::hierarchy_chain::range_contains;

    if !range_contains(&effective_range, &selection.range) {
        return selection;
    }

    let parent = match selection.parent {
        Some(parent) => {
            let parent = *parent;
            let parent_range = parent.range;
            let spliced = Some(Box::new(splice_effective_range_into_hierarchy(
                parent,
                effective_range,
                content_node,
                mapper,
            )));
            if range_contains(&parent_range, &effective_range)
                && !ranges_equal(&parent_range, &effective_range)
            {
                Some(Box::new(SelectionRange {
                    range: effective_range,
                    parent: spliced,
                }))
            } else {
                spliced
            }
        }
        None => Some(Box::new(SelectionRange {
            range: effective_range,
            parent: content_node
                .parent()
                .map(|p| Box::new(build_from_node(p, mapper))),
        })),
    };

    SelectionRange {
        range: selection.range,
        parent,
    }
}
