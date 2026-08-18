//! Runtime evaluation of Neovim query directives.

use tree_sitter::{Query, QueryMatch};

use crate::language::query_predicates::lua_gsub;
use crate::text::clamped_slice;

/// A capture's directive-adjusted byte and point range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureRange {
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: tree_sitter::Point,
    pub end_point: tree_sitter::Point,
}

/// Apply the last valid `#offset!` targeting `capture_id`.
pub(crate) fn capture_range(
    query: &Query,
    pattern_index: usize,
    capture_id: u32,
    node: tree_sitter::Node,
    source: &str,
) -> CaptureRange {
    let raw = CaptureRange {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_point: node.start_position(),
        end_point: node.end_position(),
    };
    let mut offset = None;
    for directive in query.general_predicates(pattern_index) {
        if directive.operator.as_ref() != "offset!"
            || !matches!(
                directive.args.first(),
                Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
            )
        {
            continue;
        }
        let parse = |arg: &tree_sitter::QueryPredicateArg| match arg {
            tree_sitter::QueryPredicateArg::String(value) => value.parse::<i32>().ok(),
            tree_sitter::QueryPredicateArg::Capture(_) => None,
        };
        let values = directive.args.get(1..5).and_then(|args| {
            let [start_row, start_column, end_row, end_column] = args else {
                return None;
            };
            Some((
                parse(start_row)?,
                parse(start_column)?,
                parse(end_row)?,
                parse(end_column)?,
            ))
        });
        if let Some((start_row, start_column, end_row, end_column)) = values {
            offset = Some(crate::language::injection::InjectionOffset {
                start_row,
                start_column,
                end_row,
                end_column,
            });
        }
    }
    let Some(offset) = offset else {
        return raw;
    };

    let effective = crate::analysis::offset_calculator::calculate_effective_range(
        source,
        crate::analysis::offset_calculator::ByteRange::new(raw.start_byte, raw.end_byte),
        offset,
    );
    CaptureRange {
        start_byte: effective.start,
        end_byte: effective.end,
        start_point: crate::language::injection::byte_to_point_anchored(
            source,
            effective.start,
            raw.start_byte,
            raw.start_point,
        ),
        end_point: crate::language::injection::byte_to_point_anchored(
            source,
            effective.end,
            raw.start_byte,
            raw.start_point,
        ),
    }
}

/// Return one capture's text after applying its `#gsub!` directives in query
/// order. A quantified capture is left unresolved, matching Neovim's
/// single-node requirement without letting a user query panic the server.
pub(crate) fn capture_text(
    query: &Query,
    match_: &QueryMatch,
    capture_id: u32,
    source: &str,
) -> Option<String> {
    let mut nodes = match_
        .captures
        .iter()
        .filter(|capture| capture.index == capture_id)
        .map(|capture| capture.node);
    let node = nodes.next()?;
    if nodes.next().is_some() {
        return None;
    }

    let mut text = clamped_slice(source, node.byte_range()).to_owned();
    for directive in query.general_predicates(match_.pattern_index) {
        if directive.operator.as_ref() != "gsub!"
            || !matches!(
                directive.args.first(),
                Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
            )
        {
            continue;
        }
        let (
            Some(tree_sitter::QueryPredicateArg::String(pattern)),
            Some(tree_sitter::QueryPredicateArg::String(replacement)),
        ) = (directive.args.get(1), directive.args.get(2))
        else {
            continue;
        };
        if let Some(replaced) = lua_gsub(pattern, replacement, &text) {
            text = replaced;
        }
    }
    Some(text)
}
