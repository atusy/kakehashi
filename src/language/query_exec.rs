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
                CapturedNode {
                    name: capture_names[c.index as usize].to_string(),
                    start_byte: node.start_byte(),
                    end_byte: node.end_byte(),
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
