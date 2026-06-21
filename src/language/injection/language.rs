//! Resolving the injection language for a matched pattern: static `#set!`,
//! nvim-treesitter's `#set-lang-from-info-string!`, then a dynamic
//! `@injection.language` capture.

use crate::language::predicate_accessor::{UnifiedPredicate, get_all_predicates};
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
            let lang_text = &text[capture.node.byte_range()];
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
                        let lang_text = &text[capture.node.byte_range()];
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
