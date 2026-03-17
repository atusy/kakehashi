use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use crate::error::LockResultExt;
use regex::Regex;
use tree_sitter::{Query, QueryCapture, QueryMatch};

/// Cache for compiled regexes converted from Lua patterns.
/// Avoids recompiling the same pattern on every invocation during
/// semantic token highlighting of large files.
///
/// Unbounded by design: patterns originate from static `.scm` query files,
/// not user input, so the total number of unique entries is bounded by the
/// finite set of `#lua-match?` / `#not-lua-match?` patterns across all
/// loaded languages (typically well under 100).
static LUA_REGEX_CACHE: LazyLock<Mutex<HashMap<String, Option<Regex>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

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
fn get_or_compile_lua_regex(pattern_str: &str) -> Option<Regex> {
    let mut cache = LUA_REGEX_CACHE
        .lock()
        .recover_poison("query_predicates::get_or_compile_lua_regex");

    if let Some(cached) = cache.get(pattern_str) {
        return cached.clone();
    }

    let result = compile_lua_regex(pattern_str);
    cache.insert(pattern_str.to_string(), result.clone());
    result
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

pub fn filter_captures<'a>(
    query: &Query,
    match_: &'a QueryMatch<'a, 'a>,
    text: &str,
) -> Vec<QueryCapture<'a>> {
    match_
        .captures
        .iter()
        .filter(|capture| check_predicate(query, match_, capture, text))
        .cloned()
        .collect()
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
