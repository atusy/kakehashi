//! Run a client-supplied tree-sitter query over a parsed tree and collect its
//! matches as plain byte-range data (query-execution-protocol).
//!
//! This is the grammar-level core behind the `kakehashi/query` LSP method. It is
//! kept free of LSP / `Kakehashi` concerns (no URI, no ULID minting, no
//! coordinate conversion) so it can be unit-tested with a bare grammar and so
//! the handler stays a thin adapter: run the query here, then map each capture
//! to a `NodeInfo` + LSP `Range`.
//!
//! Compilation reuses the tolerant [`QueryLoader::parse_query`] path so a query
//! whose individual patterns are valid but reference symbols absent from this
//! grammar yields the valid patterns' matches plus a `skipped` list, rather than
//! failing wholesale (query-execution-protocol §"Tolerant compilation").
//! Predicate evaluation reuses [`filter_captures`] so query results agree with
//! semantic-token highlighting, including the Neovim-flavored general predicates
//! (`#lua-match?`, `#has-ancestor?`, …).

use tree_sitter::{Language, QueryCursor, StreamingIterator, Tree};

use crate::language::query_loader::{ParseFailure, QueryLoader, SkippedPattern};
use crate::language::query_predicates::filter_captures;

/// One capture within a match: the capture name and the captured node's span.
///
/// `kind` is `&'static str` because tree-sitter interns node kinds in the
/// grammar's static data, matching the `(start, end, kind)` triple the node
/// tracker keys on (lazy-node-identity-tracking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedNode {
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: &'static str,
}

/// One query match, grouping its captures so correlated captures within a
/// pattern (e.g. `@context` and `@context.end`) stay together
/// (query-execution-protocol §"Grouping by match").
#[derive(Debug, Clone)]
pub(crate) struct MatchData {
    pub pattern_index: usize,
    pub captures: Vec<CapturedNode>,
}

/// Successful outcome: the matches plus any patterns dropped by tolerant
/// compilation.
#[derive(Debug)]
pub(crate) struct QueryExecOutcome {
    pub matches: Vec<MatchData>,
    pub skipped: Vec<SkippedPattern>,
}

/// The query string could not be compiled into any usable pattern — a client
/// error (query-execution-protocol §"Null vs. error semantics"). Per-pattern
/// detail is summarized into `reason`; partial-compilation diagnostics travel
/// the success path via [`QueryExecOutcome::skipped`] instead.
#[derive(Debug)]
pub(crate) struct QueryCompileError {
    pub reason: String,
}

/// Summarize a total compilation failure into a human-readable reason for the
/// JSON-RPC error message. For "all patterns invalid" we fold in the first
/// pattern's tree-sitter error, which is what a client debugging its `.scm`
/// actually needs.
fn describe_failure(reason: Option<&ParseFailure>, skipped: &[SkippedPattern]) -> String {
    match reason {
        Some(ParseFailure::PatternSplitFailed(msg)) => {
            format!("malformed query syntax: {msg}")
        }
        Some(ParseFailure::CombinationFailed(msg)) => {
            format!("patterns valid individually but failed combined: {msg}")
        }
        Some(ParseFailure::AllPatternsInvalid) => match skipped.first() {
            Some(first) => format!(
                "no valid patterns (e.g. line {}: {})",
                first.start_line, first.error
            ),
            None => "no valid patterns in query".to_string(),
        },
        None => "query produced no compilable patterns".to_string(),
    }
}

/// Compile `query_str` against `language` and run it over `tree`, returning the
/// matches over `text`. `match_limit` caps the number of matches returned (the
/// server's safety bound); `None` means no cap.
pub(crate) fn run_query(
    language: &Language,
    tree: &Tree,
    text: &str,
    query_str: &str,
    match_limit: Option<usize>,
) -> Result<QueryExecOutcome, QueryCompileError> {
    // Tolerant compilation: keep matches from the valid patterns and report the
    // dropped ones. `used_inheritance: false` — client queries are standalone,
    // so skipped line numbers refer to the query string as given.
    let parsed = QueryLoader::parse_query(language, query_str, false);
    let Some(query) = parsed.query else {
        return Err(QueryCompileError {
            reason: describe_failure(parsed.failure_reason.as_ref(), &parsed.skipped),
        });
    };

    let capture_names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        // Cap on returned matches (server safety bound). Break rather than
        // continue: matches arrive in a stable order, so the first `limit` are a
        // deterministic prefix.
        if match_limit.is_some_and(|limit| out.len() >= limit) {
            break;
        }

        // `filter_captures` drops captures whose general predicates fail
        // (built-in #eq?/#match?/#any-of? are already applied by `matches()`).
        let captures: Vec<CapturedNode> = filter_captures(&query, m, text)
            .map(|c| {
                let node = c.node;
                CapturedNode {
                    name: capture_names[c.index as usize].to_string(),
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    kind: node.kind(),
                }
            })
            .collect();

        // A match whose every capture was filtered out contributes nothing
        // observable; skip it so clients don't see empty match envelopes.
        if captures.is_empty() {
            continue;
        }

        out.push(MatchData {
            pattern_index: m.pattern_index,
            captures,
        });
    }

    Ok(QueryExecOutcome {
        matches: out,
        skipped: parsed.skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn rust_tree(src: &str) -> (Language, Tree) {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (language, tree)
    }

    #[test]
    fn captures_function_name() {
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);

        let outcome = run_query(
            &language,
            &tree,
            src,
            "(function_item name: (identifier) @name)",
            None,
        )
        .expect("valid query compiles");

        assert_eq!(outcome.matches.len(), 1, "one function -> one match");
        let m = &outcome.matches[0];
        assert_eq!(m.captures.len(), 1);
        let c = &m.captures[0];
        assert_eq!(c.name, "name");
        assert_eq!(&src[c.start_byte..c.end_byte], "foo");
        assert_eq!(c.kind, "identifier");
    }

    #[test]
    fn match_limit_caps_returned_matches() {
        let src = "fn a() {} fn b() {} fn c() {}";
        let (language, tree) = rust_tree(src);
        let query = "(function_item name: (identifier) @name)";

        let all = run_query(&language, &tree, src, query, None).unwrap();
        assert_eq!(all.matches.len(), 3, "three functions without a cap");

        let capped = run_query(&language, &tree, src, query, Some(2)).unwrap();
        assert_eq!(capped.matches.len(), 2, "match_limit truncates to a prefix");
        assert_eq!(
            &src[capped.matches[0].captures[0].start_byte..capped.matches[0].captures[0].end_byte],
            "a"
        );
    }

    #[test]
    fn builtin_eq_predicate_is_applied() {
        // #eq? is a built-in text predicate handled by tree-sitter's matches();
        // only the identifier literally equal to "wanted" should match.
        let src = "fn wanted() {} fn other() {}";
        let (language, tree) = rust_tree(src);
        let query = r#"((function_item name: (identifier) @name) (#eq? @name "wanted"))"#;

        let outcome = run_query(&language, &tree, src, query, None).unwrap();
        assert_eq!(outcome.matches.len(), 1);
        let c = &outcome.matches[0].captures[0];
        assert_eq!(&src[c.start_byte..c.end_byte], "wanted");
    }

    #[test]
    fn unparseable_query_is_a_compile_error() {
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);

        let err = run_query(&language, &tree, src, "(this is not balanced", None)
            .expect_err("a malformed query must not silently succeed");
        assert!(!err.reason.is_empty());
    }

    #[test]
    fn pattern_referencing_unknown_node_is_skipped_not_fatal() {
        // A valid pattern plus one referencing a node kind absent from the Rust
        // grammar: tolerant compilation keeps the good one and reports the bad.
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        let query = "(function_item name: (identifier) @good)\n(no_such_node) @bad";

        let outcome = run_query(&language, &tree, src, query, None).unwrap();
        assert_eq!(outcome.matches.len(), 1, "the valid pattern still runs");
        assert_eq!(outcome.matches[0].captures[0].name, "good");
        assert_eq!(outcome.skipped.len(), 1, "the invalid pattern is reported");
    }
}
