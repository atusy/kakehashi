use std::collections::HashSet;

use regex::Regex;
use tree_sitter::{Query, QueryCapture, QueryMatch};

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
            _ => {}
        }
    }

    true
}

/// Check lua-match? predicate - returns true if pattern matches or on error (permissive)
fn check_lua_match(arg: Option<&tree_sitter::QueryPredicateArg>, node_text: &str) -> bool {
    let Some(tree_sitter::QueryPredicateArg::String(pattern_str)) = arg else {
        return true; // No pattern arg, pass through
    };

    let Ok(parsed_pattern) = lua_pattern::parse(pattern_str) else {
        log::info!(
            target: "kakehashi::query",
            "Invalid lua-pattern: {}",
            pattern_str
        );
        return true; // Parse error, pass through
    };

    let regex_str = match lua_pattern::try_to_regex(&parsed_pattern, false, false) {
        Ok(regex_str) => regex_str,
        Err(err) => {
            log::info!(
                target: "kakehashi::query",
                "Failed to convert lua-pattern to regex: {} ({err:?})",
                pattern_str
            );
            return true; // Conversion error, pass through
        }
    };

    let re = match Regex::new(&regex_str) {
        Ok(re) => re,
        Err(err) => {
            log::info!(
                target: "kakehashi::query",
                "Failed to compile regex from lua-pattern: {} ({err:?})",
                regex_str
            );
            return true; // Regex compile error, pass through
        }
    };

    re.is_match(node_text)
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
