//! Build LSP `SelectionRange` hierarchies from Tree-sitter AST nodes, with
//! optional injection awareness (e.g. YAML in Markdown).
//!
//! Entry points: [`build`] (detects injections, falls back to AST), and the
//! injection-naive [`build_from_node`] / injection-aware
//! [`build_from_node_in_injection`]. When [`build`] sees an injection it parses
//! the injected content; on parse failure it returns `build_unparsed_fallback`.

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
    self, compute_included_ranges_clipped, has_include_children_for_pattern,
    parse_offset_directive_for_pattern, parse_with_ranges,
};
use crate::text::PositionMapper;

/// Convert tree-sitter Node to LSP Range with proper UTF-16 encoding.
///
/// Tree-sitter stores byte offsets, but LSP requires UTF-16 code units, so the
/// `mapper` performs the conversion.
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
/// Skipping ancestors with identical ranges keeps the LSP selection-range
/// hierarchy strictly expanding.
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
/// Walks from `node` to the root with no injection awareness; for
/// injection-aware building, use [`build`] instead.
pub fn build_from_node(node: Node, mapper: &PositionMapper) -> SelectionRange {
    let parent = find_distinct_parent(node, &node.byte_range())
        .map(|parent_node| Box::new(build_from_node(parent_node, mapper)));
    let range = node_to_range(node, mapper);
    SelectionRange { range, parent }
}

/// Build SelectionRange for a node that is inside injected content.
///
/// Like `build_from_node`, but adjusts positions from the injection slice back
/// to host-document coordinates via `content_start_byte`.
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
/// the set/parse/reset protocol. `effective_window` is the post-`#offset!`
/// span of `content_text` within `text` (the coordinate space of
/// `content_node`); gaps are clipped to it so child exclusion and offset
/// directives compose (#186). Without an offset the window equals the content
/// node span.
///
/// Returns `None` if `set_included_ranges` fails (after logging a warning).
fn parse_with_included_ranges(
    parser: &mut Parser,
    text: &str,
    content_text: &str,
    content_node: &Node,
    effective_window: std::ops::Range<usize>,
    include_children: bool,
    lang_name: &str,
) -> Option<Tree> {
    let included_ranges =
        compute_included_ranges_clipped(content_node, include_children, text, effective_window);

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
/// Main entry point: when the cursor is within an injection region, parses the
/// injected content for a richer hierarchy spanning multiple language ASTs;
/// otherwise falls back to pure AST traversal.
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

    let effective_window = if let Some(offset) = offset_from_query {
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        let effective = calculate_effective_range(doc_ctx.text, byte_range, offset);
        effective.start..effective.end
    } else {
        content_node.byte_range()
    };
    let content_text = &doc_ctx.text[effective_window.clone()];
    let effective_start_byte = effective_window.start;

    let include_children =
        injection_query_ref.is_some_and(|q| has_include_children_for_pattern(q, pattern_index));

    let Some(injected_tree) = parse_with_included_ranges(
        &mut parser,
        doc_ctx.text,
        content_text,
        &content_node,
        effective_window,
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
    let nested_window = if let Some(off) = offset {
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        let effective = calculate_effective_range(text, byte_range, off);
        effective.start..effective.end
    } else {
        content_node.byte_range()
    };
    let nested_text = &text[nested_window.clone()];
    let nested_effective_start_byte = parent_start_byte + nested_window.start;

    let Some(mut nested_parser) = inj_ctx.acquire_parser(&nested_lang) else {
        return build_from_node_in_injection(*node, parent_start_byte, doc_ctx.mapper);
    };

    let include_children = has_include_children_for_pattern(injection_query, pattern_index);

    let Some(nested_tree) = parse_with_included_ranges(
        &mut nested_parser,
        text,
        nested_text,
        &content_node,
        nested_window,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::injection::collect_all_injections;

    #[test]
    fn parse_with_included_ranges_strips_prefixes_inside_offset_window() {
        // #186: #offset! WITHOUT injection.include-children. The selection
        // path must still exclude blockquote `> ` prefixes when parsing the
        // post-offset window.
        //
        // Byte map:
        //   "> ```rust\n"      0..10
        //   "> let a = 1;\n"  10..23   (trimmed by the offset)
        //   "> let b = 2;\n"  23..36   (prefix at 23..25)
        //   "> ```\n"         36..42
        let md_language: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut md_parser = Parser::new();
        md_parser
            .set_language(&md_language)
            .expect("set md language");
        let text = "> ```rust\n> let a = 1;\n> let b = 2;\n> ```\n";
        let md_tree = md_parser.parse(text, None).expect("parse markdown");

        let query = tree_sitter::Query::new(
            &md_language,
            r#"
            ((fenced_code_block
               (info_string (language) @injection.language)
               (code_fence_content) @injection.content)
              (#offset! @injection.content 1 0 0 0))
            "#,
        )
        .expect("valid offset injection query");

        let regions = collect_all_injections(&md_tree.root_node(), text, Some(&query))
            .expect("should find injections");
        assert_eq!(regions.len(), 1);
        let content_node = regions[0].content_node;
        assert!(!regions[0].include_children);

        let offset = parse_offset_directive_for_pattern(&query, regions[0].pattern_index)
            .expect("offset directive");
        let byte_range = ByteRange::new(content_node.start_byte(), content_node.end_byte());
        let effective = calculate_effective_range(text, byte_range, offset);
        let content_text = &text[effective.start..effective.end];

        // Control: without child exclusion the parser sees the `> ` prefix
        // and produces an erroneous tree — this is what the old gate did.
        let rust_language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let mut control_parser = Parser::new();
        control_parser
            .set_language(&rust_language)
            .expect("set rust language");
        let control_tree = control_parser
            .parse(content_text, None)
            .expect("parse control");
        assert!(
            control_tree.root_node().has_error(),
            "control: prefix-polluted content must not parse cleanly"
        );

        let mut rust_parser = Parser::new();
        rust_parser
            .set_language(&rust_language)
            .expect("set rust language");
        let tree = parse_with_included_ranges(
            &mut rust_parser,
            text,
            content_text,
            &content_node,
            effective.start..effective.end,
            regions[0].include_children,
            "rust",
        )
        .expect("parse injected content");

        assert!(
            !tree.root_node().has_error(),
            "prefixes must be excluded inside the offset window; got tree: {}",
            tree.root_node().to_sexp()
        );
    }
}
