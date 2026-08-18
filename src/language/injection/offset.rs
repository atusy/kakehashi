//! Parsing of the `#offset!` directive that shifts injection content
//! boundaries (e.g. trimming frontmatter delimiters).

use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};
use tree_sitter::Query;

/// Represents offset adjustments for injection content boundaries
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct InjectionOffset {
    pub start_row: i32,
    pub start_column: i32,
    pub end_row: i32,
    pub end_column: i32,
}

/// Parse Neovim's offset arguments, defaulting omitted positions to zero.
pub(crate) fn parse_offset_args(
    args: &[tree_sitter::QueryPredicateArg],
) -> Option<InjectionOffset> {
    let parse = |index: usize| match args.get(index) {
        None => Some(0),
        Some(tree_sitter::QueryPredicateArg::String(value)) => value.parse::<i32>().ok(),
        Some(tree_sitter::QueryPredicateArg::Capture(_)) => None,
    };
    Some(InjectionOffset {
        start_row: parse(0)?,
        start_column: parse(1)?,
        end_row: parse(2)?,
        end_column: parse(3)?,
    })
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
    let mut offset = None;
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

        offset = Some(parse_offset_args(&pred.args[1..]).unwrap_or_default());
    }
    offset
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tree_sitter::Query;

    #[test]
    fn test_parse_offset_directive_for_pattern() {
        // Test that the pattern-aware function correctly returns
        // offsets only for the specific pattern

        // Create a query similar to markdown's injection.scm with multiple patterns
        let query_str = r#"
            ; Pattern 0: Raw string literals - NO OFFSET
            ((raw_string_literal) @injection.content
              (#set! injection.language "regex"))

            ; Pattern 1: Comments - HAS OFFSET
            ((line_comment) @injection.content
              (#set! injection.language "markdown")
              (#offset! @injection.content 1 0 -1 0))
        "#;

        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(&language, query_str).expect("valid query");

        // Pattern 0 (raw_string_literal) has NO offset
        let offset_pattern_0 = parse_offset_directive_for_pattern(&query, 0);
        assert_eq!(offset_pattern_0, None, "Pattern 0 should have no offset");

        // Pattern 1 (line_comment) HAS offset
        let offset_pattern_1 = parse_offset_directive_for_pattern(&query, 1);
        assert_eq!(
            offset_pattern_1,
            Some(InjectionOffset {
                start_row: 1,
                start_column: 0,
                end_row: -1,
                end_column: 0
            }),
            "Pattern 1 should have offset (1, 0, -1, 0)"
        );
    }

    #[test]
    fn last_offset_directive_wins() {
        let language = tree_sitter_rust::LANGUAGE.into();
        let query = Query::new(
            &language,
            r#"((line_comment) @injection.content
                 (#offset! @injection.content 0 1 0 0)
                 (#offset! @injection.content 0 2 0 -1))"#,
        )
        .unwrap();

        assert_eq!(
            parse_offset_directive_for_pattern(&query, 0),
            Some(InjectionOffset {
                start_row: 0,
                start_column: 2,
                end_row: 0,
                end_column: -1,
            })
        );
    }

    #[rstest]
    #[case::non_numeric_values("foo bar baz qux", Some(InjectionOffset::default()))]
    #[case::missing_arguments("1 0", Some(InjectionOffset { start_row: 1, start_column: 0, end_row: 0, end_column: 0 }))]
    #[case::extra_arguments("1 0 -1 0 5", Some(InjectionOffset { start_row: 1, start_column: 0, end_row: -1, end_column: 0 }))]
    #[case::mixed_valid_invalid("1 invalid -1 0", Some(InjectionOffset::default()))]
    #[case::empty_args("", Some(InjectionOffset::default()))]
    #[trace]
    fn test_offset_directive_edge_cases(
        #[case] offset_args: &str,
        #[case] expected: Option<InjectionOffset>,
    ) {
        let language = tree_sitter_rust::LANGUAGE.into();
        let query_str = if offset_args.is_empty() {
            r#"
            ((line_comment) @injection.content
              (#set! injection.language "test")
              (#offset! @injection.content))
        "#
            .to_string()
        } else {
            format!(
                r#"
            ((line_comment) @injection.content
              (#set! injection.language "test")
              (#offset! @injection.content {}))
        "#,
                offset_args
            )
        };

        let query = Query::new(&language, &query_str).expect("valid query");
        let offset = parse_offset_directive_for_pattern(&query, 0);

        assert_eq!(
            offset, expected,
            "offset_args={:?} should produce {:?}",
            offset_args, expected
        );
    }
}
