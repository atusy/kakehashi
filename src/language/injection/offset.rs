//! Parsing of the `#offset!` directive that shifts injection content
//! boundaries (e.g. trimming frontmatter delimiters).

use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};
use tree_sitter::Query;

/// Represents offset adjustments for injection content boundaries
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InjectionOffset {
    pub start_row: i32,
    pub start_column: i32,
    pub end_row: i32,
    pub end_column: i32,
}

/// The `#offset!` directive for a pattern, normalized: a directive that
/// parses to all zeros (malformed, or an explicit `#offset! … 0 0 0 0`) is a
/// no-op and is reported as `None`, so consumers never disable
/// included-range stripping — or skip raw-span fast paths — for an offset
/// that changes nothing. Use this instead of
/// [`parse_offset_directive_for_pattern`] anywhere behavior branches on the
/// offset's presence.
pub(crate) fn effective_offset_for_pattern(
    query: &Query,
    pattern_index: usize,
) -> Option<InjectionOffset> {
    parse_offset_directive_for_pattern(query, pattern_index)
        .filter(|off| *off != InjectionOffset::default())
}

/// Parses offset directive for a specific pattern in the query.
/// Returns None if the specified pattern has no #offset! directive for @injection.content.
pub(crate) fn parse_offset_directive_for_pattern(
    query: &Query,
    pattern_index: usize,
) -> Option<InjectionOffset> {
    for predicate in get_all_predicates(query, pattern_index) {
        // Skip non-offset! directives
        if predicate.operator() != "offset!" {
            continue;
        }

        // Skip non-General predicates
        let UnifiedPredicate::General(pred) = predicate else {
            continue;
        };

        // Skip if first arg is not a capture
        let Some(tree_sitter::QueryPredicateArg::Capture(capture_id)) = pred.args.first() else {
            continue;
        };

        // Skip if capture name not found or not @injection.content
        let Some(_) = query
            .capture_names()
            .get(*capture_id as usize)
            .filter(|name| **name == "injection.content")
        else {
            continue;
        };

        // Parse the 4 numeric arguments after the capture
        // Format: (#offset! @injection.content start_row start_col end_row end_col)
        let arg_count = pred.args.len();

        // Validate argument count (should be 5: capture + 4 offsets)
        if arg_count < 5 {
            log::info!(
                target: "kakehashi::query",
                "Malformed #offset! directive for pattern {}: expected 4 offset values, got {}. \
                Using default offset (0, 0, 0, 0). \
                Correct format: (#offset! @injection.content start_row start_col end_row end_col)",
                pattern_index,
                arg_count - 1 // Subtract 1 for the capture argument
            );
            return Some(InjectionOffset::default());
        }

        // Try to parse each argument as i32
        let parse_arg = |idx: usize| -> Result<i32, String> {
            if let Some(tree_sitter::QueryPredicateArg::String(s)) = pred.args.get(idx) {
                s.parse().map_err(|_| s.to_string())
            } else {
                Err(String::from("missing"))
            }
        };

        // Parse all 4 offset values
        let parse_results = [
            ("start_row", parse_arg(1)),
            ("start_col", parse_arg(2)),
            ("end_row", parse_arg(3)),
            ("end_col", parse_arg(4)),
        ];

        // Pattern match on all 4 results - more idiomatic than all(is_ok) + unwrap()
        if let [
            ("start_row", Ok(start_row)),
            ("start_col", Ok(start_col)),
            ("end_row", Ok(end_row)),
            ("end_col", Ok(end_col)),
        ] = parse_results.as_slice()
        {
            return Some(InjectionOffset {
                start_row: *start_row,
                start_column: *start_col,
                end_row: *end_row,
                end_column: *end_col,
            });
        }

        // Log which values failed to parse
        let error_details: Vec<String> = parse_results
            .into_iter()
            .filter_map(|(name, result)| result.err().map(|val| format!("{} = '{}'", name, val)))
            .collect();

        log::info!(
            target: "kakehashi::query",
            "Failed to parse #offset! directive for pattern {}: invalid values [{}]. \
            Using default offset (0, 0, 0, 0). \
            All offset values must be integers.",
            pattern_index,
            error_details.join(", ")
        );

        return Some(InjectionOffset::default());
    }
    None
}
