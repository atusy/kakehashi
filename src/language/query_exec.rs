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
///
/// `metadata` holds the pattern's capture-scoped `#set!` directives —
/// `(#set! @capture key value)` — for this capture, as `(key, value)` pairs
/// in query-file order (treesitter-directive-set!).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedNode {
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: &'static str,
    pub metadata: Vec<(String, Option<String>)>,
}

/// One query match, grouping its captures so correlated captures within a
/// pattern (e.g. `@context` and `@context.end`) stay together
/// (captures-protocol §"Result shapes").
///
/// `metadata` holds the pattern's match-level `#set!` directives — those
/// without a capture argument, `(#set! key value)` — as `(key, value)` pairs
/// in query-file order (treesitter-directive-set!). The value is `None` for
/// the bare flag form `(#set! key)`.
#[derive(Debug, Clone)]
pub(crate) struct MatchData {
    pub pattern_index: usize,
    pub captures: Vec<CapturedNode>,
    pub metadata: Vec<(String, Option<String>)>,
}

/// Run an already-compiled `query` over `tree`, collecting matches over `text`.
///
/// `byte_range` restricts matching via `QueryCursor::set_byte_range` (matches
/// whose nodes intersect the range); `None` walks the whole tree. There is
/// deliberately no match cap: silent truncation would poison the captures
/// delta lineage, and scoping is the byte range's job (captures-protocol
/// §"Considered Options").
pub(crate) fn execute_query(
    query: &Query,
    tree: &Tree,
    text: &str,
    byte_range: Option<std::ops::Range<usize>>,
) -> Vec<MatchData> {
    let capture_names = query.capture_names();
    let mut out = Vec::new();
    let mut cursor = QueryCursor::new();
    if let Some(range) = byte_range {
        cursor.set_byte_range(range);
    }
    let mut matches = cursor.matches(query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        // `#set!` directives are parsed by tree-sitter into per-pattern
        // property settings; a capture argument scopes one to that capture,
        // its absence makes it match-level (treesitter-directive-set!).
        let properties = query.property_settings(m.pattern_index);
        let metadata_for = |capture_id: Option<usize>| -> Vec<(String, Option<String>)> {
            properties
                .iter()
                .filter(|p| p.capture_id == capture_id)
                .map(|p| (p.key.to_string(), p.value.as_ref().map(|v| v.to_string())))
                .collect()
        };

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
                    metadata: metadata_for(Some(c.index as usize)),
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
            metadata: metadata_for(None),
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

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(matches.len(), 1, "one function -> one match");
        let m = &matches[0];
        assert_eq!(m.captures.len(), 1);
        let c = &m.captures[0];
        assert_eq!(c.name, "name");
        assert_eq!(&src[c.start_byte..c.end_byte], "foo");
        assert_eq!(c.kind, "identifier");
    }

    #[test]
    fn byte_range_scopes_the_walk() {
        let src = "fn a() {} fn b() {} fn c() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(&language, "(function_item name: (identifier) @name)");

        let all = execute_query(&query, &tree, src, None);
        assert_eq!(all.len(), 3, "whole-tree walk sees all three functions");

        // Bytes 10..19 cover exactly `fn b() {}`; a and c lie outside.
        let scoped = execute_query(&query, &tree, src, Some(10..19));
        assert_eq!(scoped.len(), 1, "only the function intersecting the range");
        let c = &scoped[0].captures[0];
        assert_eq!(&src[c.start_byte..c.end_byte], "b");
    }

    #[test]
    fn set_directive_surfaces_match_level_metadata() {
        // (#set! key value) without a capture sets match-level metadata
        // (treesitter-directive-set!): every match of the pattern carries it.
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @name) (#set! kind "function"))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].metadata,
            vec![("kind".to_string(), Some("function".to_string()))]
        );
    }

    #[test]
    fn set_directive_with_capture_attaches_metadata_to_that_capture() {
        // (#set! @capture key value) is capture-scoped
        // (treesitter-directive-set!): only the named capture carries it,
        // and it does not leak into the match-level metadata.
        let src = "fn foo(x: u32) {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @name (parameters) @params)
                (#set! @params kind "parameter-list"))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(matches.len(), 1);
        let m = &matches[0];
        assert!(m.metadata.is_empty(), "capture-scoped #set! is not match-level");
        let name = m.captures.iter().find(|c| c.name == "name").unwrap();
        let params = m.captures.iter().find(|c| c.name == "params").unwrap();
        assert!(name.metadata.is_empty(), "@name was not annotated");
        assert_eq!(
            params.metadata,
            vec![("kind".to_string(), Some("parameter-list".to_string()))]
        );
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

        let matches = execute_query(&query, &tree, src, None);
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

        let matches = execute_query(&query, &tree, src, None);
        assert_eq!(matches.len(), 1, "the valid pattern still runs");
        assert_eq!(matches[0].captures[0].name, "good");
    }
}
