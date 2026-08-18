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
//! path). Predicate evaluation uses [`check_match_predicates`] — Neovim's
//! `iter_matches` semantics for the Neovim-flavored general predicates
//! (`#lua-match?`, `#has-ancestor?`, …): each predicate is computed once
//! over all nodes of its capture, `not-` negates that aggregate, and one
//! failing predicate discards the match and its captures entirely.
//! Highlighting keeps its per-capture filtering (a guard capture there
//! should not kill its siblings' colors); here the match envelope is the
//! protocol unit, and `#set!` metadata must not survive a match Neovim
//! would reject (captures-protocol §"Result shapes").

use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::language::query_predicates::check_match_predicates;

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
    /// Raw node span used for node identity.
    pub start_byte: usize,
    pub end_byte: usize,
    /// Directive-adjusted span exposed as the capture range.
    pub range_start_byte: usize,
    pub range_end_byte: usize,
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
/// `byte_range` restricts results to captures whose effective ranges intersect
/// the range. Queries without runtime range directives use
/// `QueryCursor::set_byte_range` as an early pruning step; range-adjusting
/// queries walk the tree before filtering because `#offset!` can move a raw
/// node into the requested range. `None` walks the whole tree. There is
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
    let requires_effective_range_filter =
        byte_range.is_some() && crate::language::query_directives::has_range_directive(query);
    let requires_full_walk = byte_range.is_some()
        && crate::language::query_directives::has_expanding_range_directive(query);
    if let Some(range) = byte_range.clone().filter(|_| !requires_full_walk) {
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

        // One failing general predicate discards the whole match — Neovim's
        // iter_matches semantics, matching how tree-sitter's `matches()`
        // already gates the built-in #eq?/#match?/#any-of? per match.
        if !check_match_predicates(query, m, text) {
            continue;
        }

        let captures: Vec<CapturedNode> = m
            .captures
            .iter()
            .map(|c| {
                let node = c.node;
                let range =
                    crate::language::query_directives::capture_range(query, m, c.index, node, text);
                CapturedNode {
                    name: capture_names[c.index as usize].to_string(),
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
                    range_start_byte: range.start_byte,
                    range_end_byte: range.end_byte,
                    kind: node.kind(),
                    metadata: metadata_for(Some(c.index as usize)),
                }
            })
            .collect();

        // tree-sitter can yield capture-less matches for patterns whose
        // captures are all quantified-out; an empty envelope says nothing.
        if captures.is_empty() {
            continue;
        }
        // Tree-sitter's range cursor retains an entire correlated match when
        // any capture intersects. Preserve that contract after runtime range
        // evaluation instead of pruning individual captures from the match.
        if requires_effective_range_filter
            && !captures.iter().any(|capture| {
                byte_range.as_ref().is_none_or(|requested| {
                    capture.range_start_byte < requested.end
                        && capture.range_end_byte > requested.start
                })
            })
        {
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
    fn offset_directive_adjusts_capture_range_without_changing_node_span() {
        let src = r#""abc""#;
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((string_literal) @string (#offset! @string 0 1 0 -1))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(matches.len(), 1);
        let capture = &matches[0].captures[0];
        assert_eq!((capture.start_byte, capture.end_byte), (0, 5));
        assert_eq!((capture.range_start_byte, capture.range_end_byte), (1, 4));
    }

    #[test]
    fn invalid_offset_directive_keeps_the_raw_capture_range() {
        let src = r#""abc""#;
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((string_literal) @string (#offset! @string 0 10 0 0))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        let capture = &matches[0].captures[0];
        assert_eq!((capture.range_start_byte, capture.range_end_byte), (0, 5));
    }

    #[test]
    fn trim_directive_defaults_to_removing_trailing_blank_lines() {
        let src = "fn main() {}\n\n";
        let (language, tree) = rust_tree(src);
        let query = compile(&language, "((source_file) @fold (#trim! @fold))");

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(matches.len(), 1);
        let capture = &matches[0].captures[0];
        assert_eq!((capture.start_byte, capture.end_byte), (0, src.len()));
        assert_eq!(
            (capture.range_start_byte, capture.range_end_byte),
            (0, "fn main() {}".len())
        );
    }

    #[test]
    fn trim_range_takes_precedence_over_offset_metadata() {
        let src = "fn main() {}\n\n";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            "((source_file) @fold (#trim! @fold) (#offset! @fold 0 1 0 -1))",
        );

        let matches = execute_query(&query, &tree, src, None);

        let capture = &matches[0].captures[0];
        assert_eq!(
            (capture.range_start_byte, capture.range_end_byte),
            (0, "fn main() {}".len())
        );
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
    fn byte_range_uses_outward_offset_capture_span() {
        let src = "fn a() {} fn b() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            "((function_item name: (identifier) @name) (#offset! @name 0 -3 0 0))",
        );

        let scoped = execute_query(&query, &tree, src, Some(0..2));

        assert_eq!(scoped.len(), 1);
        assert_eq!(
            &src[scoped[0].captures[0].start_byte..scoped[0].captures[0].end_byte],
            "a"
        );
        assert_eq!(scoped[0].captures[0].range_start_byte, 0);
    }

    #[test]
    fn byte_range_rejects_capture_shifted_outside_viewport() {
        let src = "fn a() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            "((function_item) @item (#offset! @item 0 3 0 -5))",
        );

        assert!(execute_query(&query, &tree, src, Some(5..8)).is_empty());
    }

    #[test]
    fn byte_range_preserves_all_captures_in_an_intersecting_match() {
        let src = "fn a() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            "((function_item name: (identifier) @name body: (block) @body) (#offset! @name 0 0 0 0))",
        );

        let scoped = execute_query(&query, &tree, src, Some(3..4));

        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].captures.len(), 2);
        assert_eq!(scoped[0].captures[0].name, "name");
        assert_eq!(scoped[0].captures[1].name, "body");
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
    fn set_directive_flag_form_extracts_as_none_value() {
        // (#set! key) with no value is the flag form the wire contract maps
        // to JSON true. That mapping assumes tree-sitter's property_settings
        // exposes the missing value as None — pin it here so a change to
        // Some("") (or dropping the property) can't silently rewrite every
        // flag's wire value (Copilot review).
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @name) (#set! injection.combined))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].metadata,
            vec![("injection.combined".to_string(), None)]
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
        assert!(
            m.metadata.is_empty(),
            "capture-scoped #set! is not match-level"
        );
        let name = m.captures.iter().find(|c| c.name == "name").unwrap();
        let params = m.captures.iter().find(|c| c.name == "params").unwrap();
        assert!(name.metadata.is_empty(), "@name was not annotated");
        assert_eq!(
            params.metadata,
            vec![("kind".to_string(), Some("parameter-list".to_string()))]
        );
    }

    #[test]
    fn failing_general_predicate_drops_the_whole_match() {
        // Neovim's iter_matches gates the ENTIRE match on its predicates,
        // and #set! directives apply only after they pass
        // (treesitter-directive-set!). A guard capture whose predicate fails
        // must not leave a partial match — with the pattern's metadata
        // attached — behind (Codex review).
        let src = "fn foo(x: u32) {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @name (parameters) @params)
                (#lua-match? @params "^%(%)$")
                (#set! kind "no-args"))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert!(
            matches.is_empty(),
            "predicate failed on @params -> whole match discarded: {matches:?}"
        );
    }

    #[test]
    fn not_predicate_negates_the_aggregate_once() {
        // Neovim strips `not-` and negates the handler's MATCH-LEVEL result:
        // #not-lua-match? passes when NOT ALL nodes of the capture match —
        // not "no node matches". Here @id captures `foo` (matches ^foo$) and
        // the parameters node (doesn't), so lua-match?-over-all is false and
        // the negation keeps the match (Codex review r3).
        let src = "fn foo(x: u32) {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @id (parameters) @id)
                (#not-lua-match? @id "^foo$"))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(
            matches.len(),
            1,
            "not all @id nodes match -> negated aggregate keeps the match"
        );
        assert_eq!(matches[0].captures.len(), 2, "both @id occurrences kept");
    }

    #[test]
    fn has_parent_accepts_when_any_occurrence_satisfies() {
        // Neovim's has-parent?/has-ancestor? handlers accept when ANY node of
        // the capture has the requested parent — not every occurrence. @x
        // captures the function name (parent function_item) and the parameter
        // name (parent parameter); one hit must keep the match (Codex r3).
        let src = "fn foo(x: u32) {}";
        let (language, tree) = rust_tree(src);
        let query = compile(
            &language,
            r#"((function_item name: (identifier) @x
                  parameters: (parameters (parameter (identifier) @x)))
                (#has-parent? @x "parameter"))"#,
        );

        let matches = execute_query(&query, &tree, src, None);

        assert_eq!(
            matches.len(),
            1,
            "one @x occurrence has a parameter parent -> match kept"
        );
    }

    #[test]
    fn negated_predicate_with_non_capture_arg_rejects_the_match() {
        // Neovim's handlers index match[predicate[2]] with the raw argument:
        // a quoted "capture" finds no nodes, the handler returns vacuous
        // true, and _match_predicates' not- inversion rejects the match. A
        // typoed negated predicate must fail closed, not leak matches with
        // #set! metadata (Codex review r3).
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        let negated = compile(
            &language,
            r#"((function_item name: (identifier) @name)
                (#not-lua-match? "name" "^foo$"))"#,
        );
        assert!(
            execute_query(&negated, &tree, src, None).is_empty(),
            "vacuous true + not- inversion rejects the match"
        );

        // The positive form stays vacuously true, as in Neovim.
        let positive = compile(
            &language,
            r#"((function_item name: (identifier) @name)
                (#lua-match? "name" "^zzz$"))"#,
        );
        assert_eq!(execute_query(&positive, &tree, src, None).len(), 1);
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
    fn zero_arg_predicate_neither_panics_nor_leaks_negated_matches() {
        // A predicate with no arguments at all compiles — tree-sitter does
        // not validate general predicates — and must not crash the server
        // (Copilot review claimed `args[1..]` panics here). With no capture
        // argument the predicate selects no nodes, so the aggregate is
        // vacuously true: positive forms keep the match, `not-` forms fail
        // closed, exactly like the typo-quoted-capture case above.
        let src = "fn foo() {}";
        let (language, tree) = rust_tree(src);
        for operator in ["contains?", "has-parent?", "has-ancestor?"] {
            let positive = compile(
                &language,
                &format!("((function_item name: (identifier) @name) (#{operator}))"),
            );
            assert_eq!(
                execute_query(&positive, &tree, src, None).len(),
                1,
                "zero-arg #{operator} is vacuously true"
            );

            let negated = compile(
                &language,
                &format!("((function_item name: (identifier) @name) (#not-{operator}))"),
            );
            assert!(
                execute_query(&negated, &tree, src, None).is_empty(),
                "zero-arg #not-{operator} fails closed"
            );
        }
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
