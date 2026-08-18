//! Runtime evaluation of Neovim query directives.

use tree_sitter::{Query, QueryMatch};

use crate::language::query_predicates::lua_gsub;
use crate::text::clamped_slice;

/// Return one capture's text after applying its `#gsub!` directives in query
/// order. A quantified capture is left unresolved, matching Neovim's
/// single-node requirement without letting a user query panic the server.
pub(crate) fn capture_text(
    query: &Query,
    match_: &QueryMatch,
    capture_id: u32,
    source: &str,
) -> Option<String> {
    let mut nodes = match_
        .captures
        .iter()
        .filter(|capture| capture.index == capture_id)
        .map(|capture| capture.node);
    let node = nodes.next()?;
    if nodes.next().is_some() {
        return None;
    }

    let mut text = clamped_slice(source, node.byte_range()).to_owned();
    for directive in query.general_predicates(match_.pattern_index) {
        if directive.operator.as_ref() != "gsub!"
            || !matches!(
                directive.args.first(),
                Some(tree_sitter::QueryPredicateArg::Capture(id)) if *id == capture_id
            )
        {
            continue;
        }
        let (
            Some(tree_sitter::QueryPredicateArg::String(pattern)),
            Some(tree_sitter::QueryPredicateArg::String(replacement)),
        ) = (directive.args.get(1), directive.args.get(2))
        else {
            continue;
        };
        if let Some(replaced) = lua_gsub(pattern, replacement, &text) {
            text = replaced;
        }
    }
    Some(text)
}
