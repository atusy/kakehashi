//! Resolving the injection language for a matched pattern: static `#set!`,
//! nvim-treesitter's `#set-lang-from-info-string!`, then a dynamic
//! `@injection.language` capture.

use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};
use crate::text::clamped_slice;
use tree_sitter::{Query, QueryMatch};

/// Extracts the injection language from query properties or captures.
///
/// Tries static `#set!`, then nvim-treesitter's `#set-lang-from-info-string!`,
/// then a dynamic `@injection.language` capture, in that order.
pub(super) fn extract_injection_language(
    query: &Query,
    match_: &QueryMatch,
    text: &str,
) -> Option<String> {
    // First check for static language via #set! property
    if let Some(language) = extract_static_language(query, match_) {
        return Some(language);
    }

    // Then check for nvim-treesitter's #set-lang-from-info-string! predicate
    if let Some(language) = extract_language_from_info_string(query, match_, text) {
        return Some(language);
    }

    // Finally check for dynamic language via @injection.language capture
    extract_dynamic_language(query, match_, text)
}

/// Extracts language from #set! injection.language property
fn extract_static_language(query: &Query, match_: &QueryMatch) -> Option<String> {
    // Use unified accessor to check property settings
    for predicate in get_all_predicates(query, match_.pattern_index) {
        if let UnifiedPredicate::Property(prop) = predicate
            && prop.key.as_ref() == "injection.language"
            && let Some(value) = &prop.value
        {
            return Some(value.as_ref().to_string());
        }
    }
    None
}

/// Extracts language from @injection.language capture
fn extract_dynamic_language(query: &Query, match_: &QueryMatch, text: &str) -> Option<String> {
    for capture in match_.captures {
        if let Some(capture_name) = query.capture_names().get(capture.index as usize)
            && *capture_name == "injection.language"
        {
            // `clamped_slice` guards a stale capture node whose range no longer
            // fits `text` (out of bounds / mid-codepoint). An empty result means
            // the capture didn't resolve to real text — treat it as "no language"
            // rather than emitting an empty language id (which would create a
            // bogus injection region downstream), mirroring the info-string path.
            let lang_text = clamped_slice(text, capture.node.byte_range());
            if lang_text.is_empty() {
                return None;
            }
            return Some(lang_text.to_string());
        }
    }
    None
}

/// Extracts language from nvim-treesitter's `#set-lang-from-info-string!` predicate.
///
/// This custom predicate uses a capture's text as the injection language, commonly
/// for markdown fenced code blocks.
fn extract_language_from_info_string(
    query: &Query,
    match_: &QueryMatch,
    text: &str,
) -> Option<String> {
    // Look for #set-lang-from-info-string! predicate
    for predicate in get_all_predicates(query, match_.pattern_index) {
        if predicate.operator() == "set-lang-from-info-string!"
            && let UnifiedPredicate::General(pred) = predicate
        {
            // The predicate takes a capture reference as argument
            if let Some(tree_sitter::QueryPredicateArg::Capture(capture_id)) = pred.args.first() {
                // Find the capture in the match
                for capture in match_.captures {
                    if capture.index == *capture_id {
                        // Extract the text from the captured node as the language
                        let lang_text = clamped_slice(text, capture.node.byte_range());
                        // Normalize the language name (lowercase, trim)
                        let normalized = lang_text.trim().to_lowercase();
                        if !normalized.is_empty() {
                            return Some(normalized);
                        }
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::{Parser, QueryCursor, StreamingIterator};

    #[test]
    fn extract_dynamic_language_stale_capture_does_not_panic() {
        // #401: a capture node from a tree that no longer matches `text` must
        // not panic the `text[capture.node.byte_range()]` slice.
        let md: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md).expect("load markdown");
        let text = "```rust\nfn main() {}\n```\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md,
            "(fenced_code_block (info_string (language) @injection.language))",
        )
        .expect("valid query");

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
        let m = matches.next().expect("one match");

        // Resolve against a much shorter text, as if the document shrank before
        // the tree was refreshed: the capture's byte range no longer fits, so the
        // slice clamps to "" — reported as "no language" (None), without panicking.
        let lang = extract_dynamic_language(&query, m, "x");
        assert_eq!(lang, None);
    }

    #[test]
    fn extract_language_from_info_string_stale_capture_does_not_panic() {
        // Same stale-tree guard as above, via the #set-lang-from-info-string!
        // path (language.rs:81), which slices a capture node identically.
        let md: tree_sitter::Language = tree_sitter_md::LANGUAGE.into();
        let mut parser = Parser::new();
        parser.set_language(&md).expect("load markdown");
        let text = "```rust\nfn main() {}\n```\n";
        let tree = parser.parse(text, None).expect("parse markdown");
        let query = Query::new(
            &md,
            r#"(fenced_code_block (info_string) @_info (#set-lang-from-info-string! @_info))"#,
        )
        .expect("valid query");

        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), text.as_bytes());
        let m = matches.next().expect("one match");

        // Out-of-bounds capture range against the shorter text → empty after
        // normalization → None, but crucially no panic.
        let lang = extract_language_from_info_string(&query, m, "x");
        assert_eq!(lang, None);
    }
}
