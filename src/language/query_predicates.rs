use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, RwLock};

use crate::error::LockResultExt;
use regex::Regex;
use tree_sitter::{Query, QueryCapture, QueryMatch};

/// Cache for compiled regexes converted from Lua patterns.
/// Avoids recompiling the same pattern on every invocation during
/// semantic token highlighting of large files.
///
/// Stored as `Arc<Regex>` (not `Regex`) and shared by reference: a `Regex`
/// owns an internal pool of lazy-DFA caches, and `Regex::clone` would hand each
/// caller a *fresh, empty* pool — forcing the lazy DFA to rebuild its transition
/// table on the first match of every call. Sharing one `Arc<Regex>` keeps a
/// single pool alive across all predicate checks (the pool is internally
/// thread-safe, so the parallel injection workers reuse it too), which removes
/// the per-call `Cache::new`/`init_cache` that dominated the hot path.
///
/// A `RwLock` (not `Mutex`) because the cache is overwhelmingly read-only after
/// warmup: every predicate check on the hot path is a lookup, so a shared read
/// lock lets the parallel injection workers proceed concurrently; only the rare
/// first-compile of a pattern takes the write lock.
///
/// Unbounded by design: patterns originate from static `.scm` query files,
/// not user input, so the total number of unique entries is bounded by the
/// finite set of `#lua-match?` / `#not-lua-match?` patterns across all
/// loaded languages (typically well under 100).
static LUA_REGEX_CACHE: LazyLock<RwLock<HashMap<String, Option<Arc<Regex>>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Check if a capture passes all predicates returned by
/// [`Query::general_predicates`] for the capture's pattern.
///
/// This covers Neovim-specific predicates like `#lua-match?` that tree-sitter
/// does not handle as built-in text predicates. Built-in predicates (`#eq?`,
/// `#match?`, `#any-of?`, etc.) are auto-applied by `QueryCursor::matches()`
/// in tree-sitter 0.26+ and never appear in `general_predicates()`.
pub(crate) fn check_predicate(
    query: &Query,
    match_: &QueryMatch,
    capture: &QueryCapture,
    text: &str,
) -> bool {
    let general_predicates = query.general_predicates(match_.pattern_index);

    for predicate in general_predicates {
        // Skip predicates that don't target this capture
        let Some(tree_sitter::QueryPredicateArg::Capture(capture_id)) = predicate.args.first()
        else {
            continue;
        };
        if *capture_id != capture.index {
            continue;
        }

        let node = capture.node;
        let Some(node_text) = text.get(node.start_byte()..node.end_byte()) else {
            continue;
        };

        match predicate.operator.as_ref() {
            "lua-match?" => {
                if !check_lua_match(predicate.args.get(1), node_text) {
                    return false;
                }
            }
            "not-lua-match?" => {
                if check_lua_match(predicate.args.get(1), node_text) {
                    return false;
                }
            }
            "contains?" => {
                if !check_contains(&predicate.args[1..], node_text) {
                    return false;
                }
            }
            "has-parent?" => {
                if !check_has_parent(&predicate.args[1..], node) {
                    return false;
                }
            }
            "not-has-parent?" => {
                if check_has_parent(&predicate.args[1..], node) {
                    return false;
                }
            }
            "has-ancestor?" => {
                if !check_has_ancestor(&predicate.args[1..], node) {
                    return false;
                }
            }
            "not-has-ancestor?" => {
                if check_has_ancestor(&predicate.args[1..], node) {
                    return false;
                }
            }
            unknown => {
                log::debug!(
                    target: "kakehashi::query",
                    "Unrecognized general predicate: #{}",
                    unknown
                );
            }
        }
    }

    true
}

/// Evaluate a match's general predicates the way Neovim's `iter_matches`
/// does (`Query:_match_predicates` in runtime/lua/vim/treesitter/query.lua):
/// each predicate is computed once over **all nodes of its capture**, a
/// `not-` prefix negates that aggregate once, and one false predicate
/// rejects the whole match.
///
/// Aggregation per operator mirrors Neovim's handlers:
/// - `lua-match?` / `contains?`: ALL nodes of the capture must satisfy;
/// - `has-parent?` / `has-ancestor?`: ANY node suffices;
/// - a capture with no nodes is vacuously true;
/// - unknown operators (including the unimplemented `any-*` family) are
///   skipped permissively, as everywhere else in kakehashi.
///
/// This is deliberately distinct from [`check_predicate`]'s per-capture
/// model used by highlighting: per-node negation (`!m(a) && !m(b)`) is NOT
/// `!(m(a) && m(b))`, and per-node ALL for has-parent? is NOT Neovim's ANY —
/// both diverge exactly when a capture matches multiple nodes.
pub(crate) fn check_match_predicates(query: &Query, match_: &QueryMatch, text: &str) -> bool {
    for predicate in query.general_predicates(match_.pattern_index) {
        let (operator, should_match) = match predicate.operator.as_ref().strip_prefix("not-") {
            Some(base) => (base, false),
            None => (predicate.operator.as_ref(), true),
        };

        // A first argument that isn't a capture (e.g. a typo-quoted
        // "capture") selects no nodes — Neovim indexes match[predicate[2]]
        // with the raw argument and gets none. The handler then answers
        // vacuous true, which `not-` inverts into a rejection: a malformed
        // negated predicate fails closed instead of leaking matches.
        let capture_id = match predicate.args.first() {
            Some(tree_sitter::QueryPredicateArg::Capture(id)) => Some(*id),
            _ => None,
        };
        let nodes = || {
            match_
                .captures
                .iter()
                .filter(|c| Some(c.index) == capture_id)
                .map(|c| c.node)
        };
        // A node whose span doesn't slice as UTF-8 passes its check
        // permissively, matching check_predicate's unreadable-text skip.
        let node_text = |node: tree_sitter::Node| text.get(node.start_byte()..node.end_byte());

        let aggregate = match operator {
            "lua-match?" => nodes()
                .all(|n| node_text(n).is_none_or(|t| check_lua_match(predicate.args.get(1), t))),
            "contains?" => nodes()
                .all(|n| node_text(n).is_none_or(|t| check_contains(&predicate.args[1..], t))),
            // Neovim: vacuously true with no nodes, otherwise ANY node hit.
            "has-parent?" => {
                nodes().next().is_none()
                    || nodes().any(|n| check_has_parent(&predicate.args[1..], n))
            }
            "has-ancestor?" => {
                nodes().next().is_none()
                    || nodes().any(|n| check_has_ancestor(&predicate.args[1..], n))
            }
            unknown => {
                log::debug!(
                    target: "kakehashi::query",
                    "Unrecognized general predicate: #{}",
                    unknown
                );
                continue;
            }
        };

        if aggregate != should_match {
            return false;
        }
    }

    true
}

/// Check lua-match? predicate - returns true if pattern matches or on error (permissive)
fn check_lua_match(arg: Option<&tree_sitter::QueryPredicateArg>, node_text: &str) -> bool {
    let Some(tree_sitter::QueryPredicateArg::String(pattern_str)) = arg else {
        return true; // No pattern arg, pass through
    };

    let re = get_or_compile_lua_regex(pattern_str);
    match re {
        Some(re) => re.is_match(node_text),
        None => true, // Compilation failed, pass through (permissive)
    }
}

/// Get a cached compiled regex for a Lua pattern, or compile and cache it.
/// Returns `None` if the pattern cannot be compiled (parse/convert/compile error).
///
/// Returns an `Arc<Regex>` (a refcount bump) so the shared regex's lazy-DFA
/// cache pool is reused across calls — see [`LUA_REGEX_CACHE`].
fn get_or_compile_lua_regex(pattern_str: &str) -> Option<Arc<Regex>> {
    // Fast path: a shared read lock, so parallel injection workers look up
    // concurrently (this is the hot path — one lookup per predicate check).
    {
        let cache = LUA_REGEX_CACHE
            .read()
            .recover_poison("query_predicates::get_or_compile_lua_regex (read)");
        if let Some(cached) = cache.get(pattern_str) {
            return cached.clone();
        }
    }

    // Miss (only the first time a pattern is seen): compile *outside* the lock so
    // the CPU-bound Regex::new doesn't block other workers, then insert under a
    // brief write lock. A concurrent miss on the same pattern just recompiles;
    // `or_insert` keeps whichever landed first — the same regex either way.
    let result = compile_lua_regex(pattern_str).map(Arc::new);
    LUA_REGEX_CACHE
        .write()
        .recover_poison("query_predicates::get_or_compile_lua_regex (write)")
        .entry(pattern_str.to_string())
        .or_insert(result)
        .clone()
}

/// Compile a Lua pattern string into a Regex. Returns None on any error.
fn compile_lua_regex(pattern_str: &str) -> Option<Regex> {
    let parsed_pattern = match lua_pattern::parse(pattern_str) {
        Ok(p) => p,
        Err(_) => {
            log::info!(
                target: "kakehashi::query",
                "Invalid lua-pattern: {}",
                pattern_str
            );
            return None;
        }
    };

    let regex_str = match lua_pattern::try_to_regex(&parsed_pattern, false, false) {
        Ok(s) => s,
        Err(err) => {
            log::info!(
                target: "kakehashi::query",
                "Failed to convert lua-pattern to regex: {} ({err:?})",
                pattern_str
            );
            return None;
        }
    };

    match Regex::new(&regex_str) {
        Ok(re) => Some(re),
        Err(err) => {
            log::info!(
                target: "kakehashi::query",
                "Failed to compile regex from lua-pattern: {} ({err:?})",
                regex_str
            );
            None
        }
    }
}

/// Check contains? predicate - returns true if ALL string args are substrings of node_text.
///
/// Non-string args (captures) are skipped permissively — they don't affect the result.
/// Zero string args yields true (vacuous truth via `Iterator::all`).
fn check_contains(args: &[tree_sitter::QueryPredicateArg], node_text: &str) -> bool {
    args.iter().all(|arg| {
        let tree_sitter::QueryPredicateArg::String(s) = arg else {
            return true; // Skip non-string args (permissive)
        };
        node_text.contains(s.as_ref())
    })
}

/// Check has-parent? predicate - returns true if direct parent's kind matches ANY string arg
fn check_has_parent(args: &[tree_sitter::QueryPredicateArg], node: tree_sitter::Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    let parent_kind = parent.kind();
    args.iter().any(|arg| {
        let tree_sitter::QueryPredicateArg::String(s) = arg else {
            return false;
        };
        parent_kind == s.as_ref()
    })
}

/// Check has-ancestor? predicate - walks parent chain, true if any ancestor's kind matches any arg
fn check_has_ancestor(args: &[tree_sitter::QueryPredicateArg], node: tree_sitter::Node) -> bool {
    let kind_args: HashSet<&str> = args
        .iter()
        .filter_map(|arg| match arg {
            tree_sitter::QueryPredicateArg::String(s) => Some(s.as_ref()),
            _ => None,
        })
        .collect();

    let mut current = node.parent();
    while let Some(ancestor) = current {
        if kind_args.contains(ancestor.kind()) {
            return true;
        }
        current = ancestor.parent();
    }
    false
}

/// Yield the captures of `match_` that pass all general predicates, lazily.
///
/// Returns an iterator rather than a `Vec` so the hot token-collection loop
/// (one call per query match on every semantic-tokens request) avoids a
/// per-match heap allocation. `QueryCapture` is `Copy`, so yielding owned
/// values is free.
pub(crate) fn filter_captures<'a>(
    query: &'a Query,
    match_: &'a QueryMatch<'a, 'a>,
    text: &'a str,
) -> impl Iterator<Item = QueryCapture<'a>> + 'a {
    match_
        .captures
        .iter()
        .filter(move |capture| check_predicate(query, match_, capture, text))
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use streaming_iterator::StreamingIterator;
    use tree_sitter::{Parser, QueryCursor};

    /// Helper to collect capture texts from a query using filter_captures
    fn collect_filtered_captures(source_code: &str, query_str: &str) -> Vec<String> {
        let mut parser = Parser::new();
        let language = tree_sitter_rust::LANGUAGE.into();
        parser.set_language(&language).unwrap();

        let tree = parser.parse(source_code, None).unwrap();
        let root_node = tree.root_node();

        let query = Query::new(&language, query_str).unwrap();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root_node, source_code.as_bytes());

        let mut results = Vec::new();
        while let Some(match_) = matches.next() {
            let filtered = filter_captures(&query, match_, source_code);
            for capture in filtered {
                let text = &source_code[capture.node.start_byte()..capture.node.end_byte()];
                results.push(text.to_string());
            }
        }
        results
    }

    #[test]
    fn lua_match_predicates() {
        let source_code = r#"
            fn main() {
                let number = 123;
                let CONSTANT = 456;
                let mixed_Case = 789;
                let snake_case = 0;
            }
        "#;

        let found_numbers = collect_filtered_captures(
            source_code,
            r#"((integer_literal) @number (#lua-match? @number "^%d+$"))"#,
        );
        assert!(found_numbers.contains(&"123".to_string()));
        assert!(found_numbers.contains(&"456".to_string()));
        assert!(found_numbers.contains(&"789".to_string()));

        let found_constants = collect_filtered_captures(
            source_code,
            r#"((identifier) @constant (#lua-match? @constant "^[A-Z][A-Z_0-9]*$"))"#,
        );
        assert!(found_constants.contains(&"CONSTANT".to_string()));
        assert!(!found_constants.contains(&"mixed_Case".to_string()));
        assert!(!found_constants.contains(&"snake_case".to_string()));

        let found_words = collect_filtered_captures(
            source_code,
            r#"((identifier) @word (#lua-match? @word "^[%w_]+$"))"#,
        );
        assert!(found_words.contains(&"main".to_string()));
        assert!(found_words.contains(&"number".to_string()));
        assert!(found_words.contains(&"CONSTANT".to_string()));
        assert!(found_words.contains(&"mixed_Case".to_string()));
        assert!(found_words.contains(&"snake_case".to_string()));
    }

    #[test]
    fn lua_match_with_anchors() {
        let source_code = r#"
            fn test_function() {
                let x = 1;
            }
        "#;

        let found_funcs = collect_filtered_captures(
            source_code,
            r#"((identifier) @func (#lua-match? @func "^test"))"#,
        );
        assert!(found_funcs.contains(&"test_function".to_string()));
        assert!(!found_funcs.contains(&"x".to_string()));

        let found_suffix = collect_filtered_captures(
            source_code,
            r#"((identifier) @func_suffix (#lua-match? @func_suffix "function$"))"#,
        );
        assert!(found_suffix.contains(&"test_function".to_string()));
    }

    #[test]
    fn lua_match_quantifiers() {
        let source_code = r#"
            fn main() {
                let a = 1;
                let ab = 2;
                let abc = 3;
                let abcd = 4;
            }
        "#;

        let found = collect_filtered_captures(
            source_code,
            r#"((identifier) @multi (#lua-match? @multi "^a%l+$"))"#,
        );
        assert!(!found.contains(&"a".to_string()));
        assert!(found.contains(&"ab".to_string()));
        assert!(found.contains(&"abc".to_string()));
        assert!(found.contains(&"abcd".to_string()));

        let found_any = collect_filtered_captures(
            source_code,
            r#"((identifier) @any (#lua-match? @any "^a%l*$"))"#,
        );
        assert!(found_any.contains(&"a".to_string()));
        assert!(found_any.contains(&"ab".to_string()));
        assert!(found_any.contains(&"abc".to_string()));
        assert!(found_any.contains(&"abcd".to_string()));
    }

    #[test]
    fn not_lua_match_predicate() {
        let source_code = r#"
            fn test_fn() {}
            fn main() {}
        "#;

        let query_str = r#"((identifier) @name (#not-lua-match? @name "^test"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"main".to_string()));
        assert!(!results.contains(&"test_fn".to_string()));
    }

    #[test]
    fn contains_predicate() {
        let source_code = r#"
            fn foobar() {}
            fn foo() {}
            fn bar() {}
        "#;

        let query_str = r#"((identifier) @name (#contains? @name "oo"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"foobar".to_string()));
        assert!(results.contains(&"foo".to_string()));
        assert!(!results.contains(&"bar".to_string()));
    }

    #[test]
    fn contains_predicate_multiple_args() {
        let source_code = r#"
            fn foobar() {}
            fn foo() {}
            fn bar() {}
        "#;

        let query_str = r#"((identifier) @name (#contains? @name "foo" "bar"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"foobar".to_string()));
        assert!(!results.contains(&"foo".to_string()));
        assert!(!results.contains(&"bar".to_string()));
    }

    #[test]
    fn contains_predicate_no_args() {
        let source_code = r#"
            fn foobar() {}
        "#;

        let query_str = r#"((identifier) @name (#contains? @name))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"foobar".to_string()));
    }

    #[test]
    fn has_parent_predicate() {
        let source_code = r#"
            fn main() {
                let x = 1;
            }
        "#;

        let query_str = r#"((identifier) @name (#has-parent? @name "let_declaration"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"x".to_string()));
        assert!(!results.contains(&"main".to_string()));
    }

    #[test]
    fn not_has_parent_predicate() {
        let source_code = r#"
            fn main() {
                let x = 1;
            }
        "#;

        let query_str = r#"((identifier) @name (#not-has-parent? @name "let_declaration"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(!results.contains(&"x".to_string()));
        assert!(results.contains(&"main".to_string()));
    }

    #[test]
    fn has_ancestor_predicate() {
        let source_code = r#"
            fn main() {
                let x = 1;
            }
        "#;

        let query_str = r#"((identifier) @name (#has-ancestor? @name "function_item"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"main".to_string()));
        assert!(results.contains(&"x".to_string()));

        let query_str = r#"((identifier) @name (#has-ancestor? @name "let_declaration"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(results.contains(&"x".to_string()));
        assert!(!results.contains(&"main".to_string()));
    }

    #[test]
    fn not_has_ancestor_predicate() {
        let source_code = r#"
            fn main() {
                let x = 1;
            }
        "#;

        let query_str = r#"((identifier) @name (#not-has-ancestor? @name "let_declaration"))"#;
        let results = collect_filtered_captures(source_code, query_str);

        assert!(!results.contains(&"x".to_string()));
        assert!(results.contains(&"main".to_string()));
    }
}
