//! Runtime evaluation of Neovim query directives.

use tree_sitter::{Query, QueryMatch, QueryPredicate};

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

/// Whether any valid `#offset!` in the query moves a row boundary.
///
/// Row movement searches for newlines beyond the captured node. Injected-layer
/// match caches hash only the layer's outer span, so callers use this to avoid
/// reusing results whose range calculation may inspect unhashed host bytes.
pub(crate) fn has_row_offset_directive(query: &Query) -> bool {
    (0..query.pattern_count()).any(|pattern_index| {
        query
            .general_predicates(pattern_index)
            .iter()
            .any(|directive| {
                if directive.operator.as_ref() != "offset!" {
                    return false;
                }
                [1, 3].into_iter().any(|index| {
                    matches!(
                        directive.args.get(index),
                        Some(tree_sitter::QueryPredicateArg::String(value))
                            if value.parse::<i32>().is_ok_and(|row| row != 0)
                    )
                })
            })
    })
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
    capture_range_for_directives(
        query.general_predicates(match_.pattern_index),
        match_,
        capture_id,
        node,
        source,
    )
}

fn capture_range_for_directives(
    directives: &[QueryPredicate],
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
    let mut is_single_capture = None;
    for directive in directives {
        if !matches!(
            directive.args.first(),
            Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
        ) {
            continue;
        }
        if directive.operator.as_ref() == "trim!" {
            let is_single_capture = *is_single_capture.get_or_insert_with(|| {
                match_
                    .captures
                    .iter()
                    .filter(|capture| capture.index == capture_id)
                    .take(2)
                    .count()
                    == 1
            });
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
        offset = crate::language::injection::parse_offset_args(&directive.args[1..]);
    }

    if let Some(range) = trimmed {
        return range;
    }

    let Some(offset) = offset else { return raw };
    let adjusted_start = (
        raw.start_point.row as i128 + offset.start_row as i128,
        raw.start_point.column as i128 + offset.start_column as i128,
    );
    let adjusted_end = (
        raw.end_point.row as i128 + offset.end_row as i128,
        raw.end_point.column as i128 + offset.end_column as i128,
    );
    if adjusted_start > adjusted_end {
        return raw;
    }

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
    let whitespace_only = |line: &str| line.bytes().all(|byte| byte.is_ascii_whitespace());
    let mut linewise_end = text.len();
    if trim_end_lines {
        loop {
            let line_start = text[..linewise_end]
                .rfind('\n')
                .map_or(0, |newline| newline + 1);
            if !whitespace_only(&text[line_start..linewise_end]) {
                break;
            }
            if line_start == 0 {
                linewise_end = 0;
                break;
            }
            linewise_end = line_start - 1;
        }
    }

    let mut end = if linewise_end == 0 && trim_end_lines {
        if trim_end_columns {
            0
        } else {
            return None;
        }
    } else {
        linewise_end
    };
    if trim_end_columns && linewise_end != 0 {
        let line_start = text[..linewise_end]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
        end = line_start
            + text[line_start..linewise_end]
                .trim_end_matches(|character: char| character.is_ascii_whitespace())
                .len();
    }

    let mut start = 0;
    if trim_start_lines {
        while start < linewise_end {
            let line_end = text[start..linewise_end]
                .find('\n')
                .map_or(linewise_end, |newline| start + newline);
            if !whitespace_only(&text[start..line_end]) {
                break;
            }
            start = if line_end < linewise_end {
                line_end + 1
            } else {
                linewise_end
            };
        }
    }
    let start_line_limit = if linewise_end == 0 && trim_end_lines {
        text.len()
    } else {
        linewise_end
    };
    if trim_start_columns && start < start_line_limit {
        let line_end = text[start..start_line_limit]
            .find('\n')
            .map_or(start_line_limit, |newline| start + newline);
        let line = &text[start..line_end];
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

/// Whether this pattern can change the text observed for `capture_id`.
pub(crate) fn has_text_directive(query: &Query, pattern_index: usize, capture_id: u32) -> bool {
    query
        .general_predicates(pattern_index)
        .iter()
        .any(|directive| {
            matches!(directive.operator.as_ref(), "gsub!" | "offset!" | "trim!")
                && matches!(
                    directive.args.first(),
                    Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
                )
        })
}

/// Return one capture's text after applying its runtime directives in query
/// order. Once `#gsub!` materializes text, later range directives do not alter
/// it, matching Neovim's `metadata.text` precedence. A quantified capture with
/// `#gsub!` is left unresolved rather than panicking the server.
pub(crate) fn capture_text(
    query: &Query,
    match_: &QueryMatch,
    capture_id: u32,
    source: &str,
) -> Option<String> {
    let directives = query.general_predicates(match_.pattern_index);
    let has_gsub = directives.iter().any(|directive| {
        directive.operator.as_ref() == "gsub!"
            && matches!(
                directive.args.first(),
                Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
            )
    });
    let mut nodes = match_
        .captures
        .iter()
        .filter(|capture| capture.index == capture_id)
        .map(|capture| capture.node);
    let node = nodes.next()?;
    if has_gsub && nodes.next().is_some() {
        return None;
    }

    let mut text = None;
    for (index, directive) in directives.iter().enumerate() {
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
        let input = text.get_or_insert_with(|| {
            let range = capture_range_for_directives(
                &directives[..index],
                match_,
                capture_id,
                node,
                source,
            );
            clamped_slice(source, range.start_byte..range.end_byte).to_owned()
        });
        if let Some(replaced) = lua_gsub(pattern, replacement, input) {
            *input = replaced;
        }
    }
    if let Some(text) = text {
        return Some(text);
    }
    let range = capture_range_for_directives(directives, match_, capture_id, node, source);
    Some(clamped_slice(source, range.start_byte..range.end_byte).to_owned())
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

    #[test]
    fn end_charwise_trim_can_collapse_an_all_whitespace_range() {
        let source = " \n\t";
        let raw = CaptureRange {
            start_byte: 0,
            end_byte: source.len(),
            start_point: tree_sitter::Point::new(0, 0),
            end_point: tree_sitter::Point::new(1, 1),
        };

        let range = trim_range(source, raw, (false, false, true, true)).unwrap();

        assert_eq!((range.start_byte, range.end_byte), (0, 0));
        assert_eq!(range.start_point, tree_sitter::Point::new(0, 0));
        assert_eq!(range.end_point, tree_sitter::Point::new(0, 0));
    }

    #[test]
    fn trim_rejects_an_inverted_all_whitespace_range() {
        let source = "  ";
        let raw = CaptureRange {
            start_byte: 0,
            end_byte: source.len(),
            start_point: tree_sitter::Point::new(0, 0),
            end_point: tree_sitter::Point::new(0, 2),
        };

        assert_eq!(trim_range(source, raw, (false, true, true, true)), None);
    }

    #[test]
    fn detects_only_row_bearing_offsets_as_external_scan_dependencies() {
        let language: tree_sitter::Language = tree_sitter_rust::LANGUAGE.into();
        let query = |operator: &str, args: &str| {
            Query::new(
                &language,
                &format!("((string_literal) @string (#{operator} @string {args}))"),
            )
            .unwrap()
        };

        assert!(has_row_offset_directive(&query("offset!", "1 0 0 0")));
        assert!(has_row_offset_directive(&query("offset!", "0 0 -1 0")));
        assert!(!has_row_offset_directive(&query("offset!", "0 1 0 -1")));
        assert!(!has_row_offset_directive(&query("trim!", "1 1 1 1")));
    }
}
