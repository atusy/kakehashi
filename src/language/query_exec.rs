//! Execute a precompiled tree-sitter query over a parsed tree and collect its
//! matches as plain byte-range data (captures-protocol).
//!
//! This is the grammar-level core behind the `kakehashi/captures/*` LSP
//! methods. It is kept free of LSP / `Kakehashi` concerns (no URI, no ULID
//! minting, no coordinate conversion) so it can be unit-tested with a bare
//! grammar and so the handlers stay thin adapters: execute here, then map
//! each capture to a `NodeInfo` + LSP `Range`.
//!
//! Compilation is the caller's job (the handlers load kind queries through
//! [`QueryLoader`](crate::language::query_loader::QueryLoader)'s tolerant
//! path). Predicate evaluation reuses [`filter_captures`] so capture results
//! agree with semantic-token highlighting, including the Neovim-flavored
//! general predicates (`#lua-match?`, `#has-ancestor?`, …).

use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

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
/// (captures-protocol §"Result shapes").
#[derive(Debug, Clone)]
pub(crate) struct MatchData {
    pub pattern_index: usize,
    pub captures: Vec<CapturedNode>,
}

/// Run an already-compiled `query` over `tree`, collecting matches over `text`.
///
/// `byte_range` restricts matching via `QueryCursor::set_byte_range` (matches
/// whose nodes intersect the range); `None` walks the whole tree.
/// `match_limit` caps the number of matches returned; `None` means no cap.
pub(crate) fn execute_query(
    query: &Query,
    tree: &Tree,
    text: &str,
    byte_range: Option<std::ops::Range<usize>>,
    match_limit: Option<usize>,
) -> Vec<MatchData> {
    let capture_names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    if let Some(range) = byte_range {
        cursor.set_byte_range(range);
    }
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        // Cap on returned matches (server safety bound). Break rather than
        // continue: matches arrive in a stable order, so the first `limit` are a
        // deterministic prefix.
        if match_limit.is_some_and(|limit| out.len() >= limit) {
            break;
        }

        // `filter_captures` drops captures whose general predicates fail
        // (built-in #eq?/#match?/#any-of? are already applied by `matches()`).
        let captures: Vec<CapturedNode> = filter_captures(query, m, text)
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

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::query_loader::QueryLoader;
    use tree_sitter::{Language, Parser};

    fn rust_tree(src: &str) -> (Language, Tree) {
        let language: Language = tree_sitter_rust::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        (language, tree)
    }

    /// Compile through the same tolerant path the captures handlers use.
    fn compile(language: &Language, query_str: &str) -> Query {
        QueryLoader::parse_query(language, query_str, false)
            .query
            .expect("query compiles")
    }

    #[test]
    fn captures_function_name() {
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(&language, "(function_item name: (identifier) @name)");

        let matches = execute_query(&query, &tree, src, None, None);

        assert_eq!(matches.len(), 1, "one function -> one match");
        let m = &matches[0];
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
        let query = compile(&language, "(function_item name: (identifier) @name)");

        let all = execute_query(&query, &tree, src, None, None);
        assert_eq!(all.len(), 3, "three functions without a cap");

        let capped = execute_query(&query, &tree, src, None, Some(2));
        assert_eq!(capped.len(), 2, "match_limit truncates to a prefix");
        assert_eq!(
            &src[capped[0].captures[0].start_byte..capped[0].captures[0].end_byte],
            "a"
        );
    }

    #[test]
    fn byte_range_scopes_the_walk() {
        let src = "fn a() {} fn b() {} fn c() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(&language, "(function_item name: (identifier) @name)");

        // Bytes 10..19 cover exactly `fn b() {}`; a and c lie outside.
        let scoped = execute_query(&query, &tree, src, Some(10..19), None);
        assert_eq!(scoped.len(), 1, "only the function intersecting the range");
        let c = &scoped[0].captures[0];
        assert_eq!(&src[c.start_byte..c.end_byte], "b");
    }

    #[test]
    fn builtin_eq_predicate_is_applied() {
        // #eq? is a built-in text predicate handled by tree-sitter's matches();
        // only the identifier literally equal to "wanted" should match.
        let src = "fn wanted() {} fn other() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @name) (#eq? @name "wanted"))"#,
        );

        let matches = execute_query(&query, &tree, src, None, None);
        assert_eq!(matches.len(), 1);
        let c = &matches[0].captures[0];
        assert_eq!(&src[c.start_byte..c.end_byte], "wanted");
    }

    #[test]
    fn tolerant_compilation_skips_invalid_patterns() {
        // A valid pattern plus one referencing a node kind absent from the Rust
        // grammar: tolerant compilation keeps the good one and reports the bad
        // (the handlers surface `skipped` to the client).
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        let parsed = QueryLoader::parse_query(
            &language,
            "(function_item name: (identifier) @good)\n(no_such_node) @bad",
            false,
        );

        let query = parsed.query.expect("valid pattern still compiles");
        assert_eq!(parsed.skipped.len(), 1, "the invalid pattern is reported");

        let matches = execute_query(&query, &tree, src, None, None);
        assert_eq!(matches.len(), 1, "the valid pattern still runs");
        assert_eq!(matches[0].captures[0].name, "good");
    }
}
