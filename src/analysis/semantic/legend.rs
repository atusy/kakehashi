//! Semantic token legend and capture mapping utilities.
//!
//! This module defines the semantic token types and modifiers supported by the LSP,
//! and provides functions to map tree-sitter capture names to LSP token types.

use crate::config::{CaptureMappings, WILDCARD_KEY};
use tower_lsp_server::ls_types::{SemanticTokenModifier, SemanticTokenType};

/// Semantic token types supported by the LSP legend.
pub const LEGEND_TYPES: &[SemanticTokenType] = &[
    SemanticTokenType::COMMENT,
    SemanticTokenType::KEYWORD,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::REGEXP,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::NAMESPACE,
    SemanticTokenType::TYPE,
    SemanticTokenType::STRUCT,
    SemanticTokenType::CLASS,
    SemanticTokenType::INTERFACE,
    SemanticTokenType::ENUM,
    SemanticTokenType::ENUM_MEMBER,
    SemanticTokenType::TYPE_PARAMETER,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::METHOD,
    SemanticTokenType::MACRO,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::PROPERTY,
    SemanticTokenType::EVENT,
    SemanticTokenType::MODIFIER,
    SemanticTokenType::DECORATOR,
];

/// Semantic token modifiers supported by the LSP legend.
pub const LEGEND_MODIFIERS: &[SemanticTokenModifier] = &[
    SemanticTokenModifier::DECLARATION,
    SemanticTokenModifier::DEFINITION,
    SemanticTokenModifier::READONLY,
    SemanticTokenModifier::STATIC,
    SemanticTokenModifier::DEPRECATED,
    SemanticTokenModifier::ABSTRACT,
    SemanticTokenModifier::ASYNC,
    SemanticTokenModifier::MODIFICATION,
    SemanticTokenModifier::DOCUMENTATION,
    SemanticTokenModifier::DEFAULT_LIBRARY,
];

/// Result of mapping a tree-sitter capture name to a semantic token role.
#[derive(Debug, PartialEq)]
pub(super) enum CaptureResult {
    /// Matched or user-mapped token type — emitted as a semantic token.
    Mapped(String),
    /// Unknown base type — transparent breakpoint-only token (not emitted).
    Transparent,
    /// `@none` capture — pre-processed to punch holes in parent tokens.
    NoneCapture,
    /// User explicitly suppressed via `""` mapping — filtered entirely.
    Suppressed,
}

/// Resolve a tree-sitter capture name to its semantic token role.
///
/// Looks up the capture name in user-configured mappings (filetype-specific,
/// then wildcard), and falls back to the built-in legend.
///
/// # Arguments
/// * `capture_name` - The original capture name from the tree-sitter query
/// * `filetype` - The filetype of the document being processed
/// * `capture_mappings` - The full capture mappings configuration
///
/// # Returns
/// - `CaptureResult::Mapped(name)` for known token types (base type in LEGEND_TYPES)
/// - `CaptureResult::Transparent` for unknown types — breakpoint-only tokens
/// - `CaptureResult::NoneCapture` for `@none` captures — handled by @none pre-processing
/// - `CaptureResult::Suppressed` when user explicitly maps a capture to `""` (suppress entirely)
pub(super) fn resolve_capture(
    capture_name: &str,
    filetype: Option<&str>,
    capture_mappings: Option<&CaptureMappings>,
) -> CaptureResult {
    if let Some(mappings) = capture_mappings {
        // Try filetype-specific mapping first
        if let Some(ft) = filetype
            && let Some(lang_mappings) = mappings.get(ft)
            && let Some(mapped) = lang_mappings.highlights.get(capture_name)
        {
            // Explicit mapping to empty string means "filter this capture"
            return if mapped.is_empty() {
                CaptureResult::Suppressed
            } else {
                CaptureResult::Mapped(mapped.clone())
            };
        }

        // Try wildcard mapping
        if let Some(wildcard_mappings) = mappings.get(WILDCARD_KEY)
            && let Some(mapped) = wildcard_mappings.highlights.get(capture_name)
        {
            // Explicit mapping to empty string means "filter this capture"
            return if mapped.is_empty() {
                CaptureResult::Suppressed
            } else {
                CaptureResult::Mapped(mapped.clone())
            };
        }
    }

    // @none is a special nvim-treesitter convention: it resets parent
    // highlighting within a region (e.g., `(interpolation) @none` punches
    // holes in `@string` for f-string interpolation). Recognized here so
    // it participates in sweep line overlap resolution, then filtered out
    // before delta encoding in finalize_tokens().
    if capture_name == "none" {
        return CaptureResult::NoneCapture;
    }

    // No mapping found - check if the base type is in SemanticTokensLegend.
    // Known types are returned as-is; unknown types return Transparent to create
    // tokens that generate sweep-line breakpoints without competing for winner selection.
    // split('.') on a non-empty string always yields at least one element
    let base_type = capture_name
        .split('.')
        .next()
        .expect("split always yields at least one element");
    if LEGEND_TYPES.iter().any(|t| t.as_str() == base_type) {
        CaptureResult::Mapped(capture_name.to_string())
    } else {
        // Transparent token: participates in sweep line as a breakpoint
        // generator but is excluded from winner selection (no token emitted).
        CaptureResult::Transparent
    }
}

/// Map capture names from tree-sitter queries to LSP semantic token types and modifiers
///
/// Capture names can be in the format "type.modifier1.modifier2" where:
/// - The first part is the token type (e.g., "variable", "function")
/// - Following parts are modifiers (e.g., "readonly", "defaultLibrary")
///
/// Returns `None` for unknown token types (not in LEGEND_TYPES).
/// Unknown modifiers are ignored.
pub(super) fn map_capture_to_token_type_and_modifiers(capture_name: &str) -> Option<(u32, u32)> {
    let parts: Vec<&str> = capture_name.split('.').collect();
    let token_type_name = parts.first().copied().filter(|s| !s.is_empty())?;

    let token_type_index = LEGEND_TYPES
        .iter()
        .position(|t| t.as_str() == token_type_name)? as u32;

    let mut modifiers_bitset = 0u32;
    for modifier_name in &parts[1..] {
        if let Some(index) = LEGEND_MODIFIERS
            .iter()
            .position(|m| m.as_str() == *modifier_name)
        {
            modifiers_bitset |= 1 << index;
        }
    }

    Some((token_type_index, modifiers_bitset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QueryTypeMappings;
    use rstest::rstest;
    use std::collections::HashMap;

    #[test]
    fn test_legend_types_includes_keyword() {
        assert!(LEGEND_TYPES.iter().any(|t| t.as_str() == "keyword"));
    }

    #[test]
    fn test_legend_modifiers_includes_readonly() {
        assert!(LEGEND_MODIFIERS.iter().any(|m| m.as_str() == "readonly"));
    }

    // PBI-152: Wildcard Config Inheritance for captureMappings

    #[test]
    fn resolve_capture_uses_wildcard_merge() {
        // ADR-0011: When both wildcard and specific key exist, merge them
        // This test verifies that resolve_capture correctly inherits
        // mappings from wildcard when the specific key doesn't have them
        let mut mappings = CaptureMappings::new();

        // Wildcard has "variable" and "function" mappings
        let mut wildcard_highlights = HashMap::new();
        wildcard_highlights.insert("variable".to_string(), "variable".to_string());
        wildcard_highlights.insert("function".to_string(), "function".to_string());

        mappings.insert(
            WILDCARD_KEY.to_string(),
            QueryTypeMappings {
                highlights: wildcard_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        // Rust only has "type.builtin" - should inherit "variable" and "function" from wildcard
        let mut rust_highlights = HashMap::new();
        rust_highlights.insert(
            "type.builtin".to_string(),
            "type.defaultLibrary".to_string(),
        );

        mappings.insert(
            "rust".to_string(),
            QueryTypeMappings {
                highlights: rust_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        // Test: "variable" should be inherited from wildcard for "rust"
        let result = resolve_capture("variable", Some("rust"), Some(&mappings));
        assert_eq!(
            result,
            CaptureResult::Mapped("variable".to_string()),
            "Should inherit 'variable' mapping from wildcard for 'rust'"
        );

        // Test: "type.builtin" should use rust-specific mapping
        let result = resolve_capture("type.builtin", Some("rust"), Some(&mappings));
        assert_eq!(
            result,
            CaptureResult::Mapped("type.defaultLibrary".to_string()),
            "Should use rust-specific 'type.builtin' mapping"
        );

        // Test: "function" should be inherited from wildcard for "rust"
        let result = resolve_capture("function", Some("rust"), Some(&mappings));
        assert_eq!(
            result,
            CaptureResult::Mapped("function".to_string()),
            "Should inherit 'function' mapping from wildcard for 'rust'"
        );
    }

    #[test]
    fn resolve_capture_explicit_empty_string_suppresses() {
        // User maps "variable" → "" in their config — must return Suppressed, not Transparent
        let mut highlights = HashMap::new();
        highlights.insert("variable".to_string(), String::new());
        let mut mappings = CaptureMappings::new();
        mappings.insert(
            "rust".to_string(),
            QueryTypeMappings {
                highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );
        assert_eq!(
            resolve_capture("variable", Some("rust"), Some(&mappings)),
            CaptureResult::Suppressed,
        );
    }

    #[rstest]
    #[case::unknown_spell("spell", CaptureResult::Transparent)]
    #[case::unknown_nospell("nospell", CaptureResult::Transparent)]
    #[case::unknown_conceal("conceal", CaptureResult::Transparent)]
    #[case::unknown_markup("markup", CaptureResult::Transparent)]
    #[case::unknown_other("unknown", CaptureResult::Transparent)]
    #[case::known_comment("comment", CaptureResult::Mapped("comment".to_string()))]
    #[case::known_keyword("keyword", CaptureResult::Mapped("keyword".to_string()))]
    #[case::known_variable_with_modifier("variable.readonly", CaptureResult::Mapped("variable.readonly".to_string()))]
    #[case::none_is_recognized("none", CaptureResult::NoneCapture)]
    fn resolve_capture_known_and_unknown_types(
        #[case] capture_name: &str,
        #[case] expected: CaptureResult,
    ) {
        assert_eq!(resolve_capture(capture_name, None, None), expected);
    }

    #[rstest]
    #[case::comment("comment", Some((0, 0)))]
    #[case::keyword("keyword", Some((1, 0)))]
    #[case::function("function", Some((14, 0)))]
    #[case::variable("variable", Some((17, 0)))]
    #[case::unknown("unknown", None)]
    #[case::spell("spell", None)]
    #[case::markup("markup", None)]
    #[case::empty("", None)]
    #[case::variable_readonly("variable.readonly", Some((17, 1 << 2)))]
    #[case::function_async("function.async", Some((14, 1 << 6)))]
    #[case::variable_readonly_default_library("variable.readonly.defaultLibrary", Some((17, (1 << 2) | (1 << 9))))]
    #[case::function_unknown_modifier_async("function.unknownModifier.async", Some((14, 1 << 6)))]
    fn test_map_capture_to_token_type_and_modifiers(
        #[case] capture_name: &str,
        #[case] expected: Option<(u32, u32)>,
    ) {
        let result = map_capture_to_token_type_and_modifiers(capture_name);
        match expected {
            None => assert_eq!(result, None),
            Some((expected_type, expected_modifiers)) => {
                let (token_type, modifiers) = result.unwrap();
                assert_eq!(token_type, expected_type);
                assert_eq!(modifiers, expected_modifiers);
            }
        }
    }
}
