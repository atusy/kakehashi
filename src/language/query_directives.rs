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

/// Apply Neovim's `#offset!` and `#trim!` range metadata for `capture_id`.
/// A valid trim range takes precedence over an offset, as in
/// `vim.treesitter.get_range()`.
pub(crate) fn capture_range(
    query: &Query,
    match_: &QueryMatch,
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
    let mut trimmed = None;
    let is_single_capture = match_
        .captures
        .iter()
        .filter(|capture| capture.index == capture_id)
        .count()
        == 1;
    for directive in query.general_predicates(match_.pattern_index) {
        if !matches!(
            directive.args.first(),
            Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
        ) {
            continue;
        }
        if directive.operator.as_ref() == "trim!" {
            let enabled = |index: usize| {
                matches!(
                    directive.args.get(index),
                    Some(tree_sitter::QueryPredicateArg::String(value)) if value.as_ref() == "1"
                )
            };
            if is_single_capture
                && let Some(range) = trim_range(
                    source,
                    raw,
                    (
                        enabled(1),
                        enabled(2),
                        enabled(3) || directive.args.get(1).is_none(),
                        enabled(4),
                    ),
                )
            {
                trimmed = Some(range);
            }
            continue;
        }
        if directive.operator.as_ref() != "offset!" {
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

    if let Some(range) = trimmed {
        return range;
    }

    let Some(offset) = offset else { return raw };

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

fn trim_range(
    source: &str,
    raw: CaptureRange,
    (trim_start_lines, trim_start_columns, trim_end_lines, trim_end_columns): (
        bool,
        bool,
        bool,
        bool,
    ),
) -> Option<CaptureRange> {
    let text = clamped_slice(source, raw.start_byte..raw.end_byte);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut next_start = 0;
    for line in &lines {
        starts.push(next_start);
        next_start += line.len() + 1;
    }

    let whitespace_only = |line: &str| line.bytes().all(|byte| byte.is_ascii_whitespace());
    let mut start_index = 0;
    let mut end_index = lines.len();

    if trim_end_lines {
        while end_index > 0 && whitespace_only(lines[end_index - 1]) {
            end_index -= 1;
        }
    }

    let mut end = if end_index == lines.len() {
        text.len()
    } else if end_index == 0 {
        return None;
    } else {
        starts[end_index - 1] + lines[end_index - 1].len()
    };
    if trim_end_columns {
        if end_index == 0 {
            end = 0;
        } else {
            let line = lines[end_index - 1];
            end = starts[end_index - 1]
                + line
                    .trim_end_matches(|character: char| character.is_ascii_whitespace())
                    .len();
        }
    }

    if trim_start_lines {
        while start_index < end_index && whitespace_only(lines[start_index]) {
            start_index += 1;
        }
    }
    let mut start = starts.get(start_index).copied().unwrap_or(text.len());
    if trim_start_columns && let Some(line) = lines.get(start_index) {
        start += line.len()
            - line
                .trim_start_matches(|character: char| character.is_ascii_whitespace())
                .len();
    }
    if start > end {
        return None;
    }

    let start_byte = raw.start_byte + start;
    let end_byte = raw.start_byte + end;
    Some(CaptureRange {
        start_byte,
        end_byte,
        start_point: crate::language::injection::byte_to_point_anchored(
            source,
            start_byte,
            raw.start_byte,
            raw.start_point,
        ),
        end_point: crate::language::injection::byte_to_point_anchored(
            source,
            end_byte,
            raw.start_byte,
            raw.start_point,
        ),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_range_honors_all_linewise_and_charwise_flags() {
        let source = " \n  fn main() {}  \n\t";
        let raw = CaptureRange {
            start_byte: 0,
            end_byte: source.len(),
            start_point: tree_sitter::Point::new(0, 0),
            end_point: tree_sitter::Point::new(2, 1),
        };

        let range = trim_range(source, raw, (true, true, true, true)).unwrap();

        assert_eq!(&source[range.start_byte..range.end_byte], "fn main() {}");
        assert_eq!(range.start_point, tree_sitter::Point::new(1, 2));
        assert_eq!(range.end_point, tree_sitter::Point::new(1, 14));
    }
}
