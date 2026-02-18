use regex::Regex;
use tree_sitter::{Query, QueryCapture, QueryMatch};

/// Check if a capture passes all general predicates (e.g., `#lua-match?`, `#eq?`).
///
/// Tree-sitter handles built-in predicates (`#eq?`, `#match?`, `#any-of?`) at the
/// cursor level, but Neovim-specific predicates like `#lua-match?` are "general
/// predicates" that require explicit evaluation.
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
        let node_text = &text[node.start_byte()..node.end_byte()];

        match predicate.operator.as_ref() {
            "lua-match?" => {
                if !check_lua_match(predicate.args.get(1), node_text) {
                    return false;
                }
            }
            "match?" => {
                if let Some(tree_sitter::QueryPredicateArg::String(pattern_str)) =
                    predicate.args.get(1)
                    && let Ok(re) = Regex::new(pattern_str)
                    && !re.is_match(node_text)
                {
                    return false;
                }
            }
            "eq?" => {
                if !check_eq(predicate.args.get(1), node_text, match_, text) {
                    return false;
                }
            }
            "not-eq?" => {
                if !check_not_eq(predicate.args.get(1), node_text, match_, text) {
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

/// Check eq? predicate - returns true if values are equal
fn check_eq(
    arg: Option<&tree_sitter::QueryPredicateArg>,
    node_text: &str,
    match_: &QueryMatch,
    text: &str,
) -> bool {
    let Some(value_arg) = arg else {
        return true; // No arg, pass through
    };

    get_predicate_arg_text(value_arg, match_, text).is_none_or(|other_text| node_text == other_text)
}

/// Check not-eq? predicate - returns true if values are not equal
fn check_not_eq(
    arg: Option<&tree_sitter::QueryPredicateArg>,
    node_text: &str,
    match_: &QueryMatch,
    text: &str,
) -> bool {
    let Some(value_arg) = arg else {
        return true; // No arg, pass through
    };

    get_predicate_arg_text(value_arg, match_, text) != Some(node_text)
}

fn get_predicate_arg_text<'a>(
    arg: &'a tree_sitter::QueryPredicateArg,
    match_: &'a QueryMatch,
    text: &'a str,
) -> Option<&'a str> {
    match arg {
        tree_sitter::QueryPredicateArg::String(value_str) => Some(value_str.as_ref()),
        tree_sitter::QueryPredicateArg::Capture(other_capture_id) => {
            let other_capture = match_
                .captures
                .iter()
                .find(|c| c.index == *other_capture_id)?;
            let other_node = other_capture.node;
            Some(&text[other_node.start_byte()..other_node.end_byte()])
        }
    }
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
