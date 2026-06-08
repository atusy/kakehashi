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
    /// Matched or user-mapped token type, pre-resolved to its legend
    /// `(token_type, modifiers)` indices — emitted as a semantic token.
    Mapped(u32, u32),
    /// User-config mapping to a value whose base type is not in the legend.
    /// Carries the offending name so the encode stage can log it. Competes in
    /// the sweep line like a mapped token but emits nothing (historical
    /// behavior); kept off the hot path since it only occurs on misconfig.
    MappedUnknown(String),
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
            return resolve_user_mapping(mapped);
        }

        // Try wildcard mapping
        if let Some(wildcard_mappings) = mappings.get(WILDCARD_KEY)
            && let Some(mapped) = wildcard_mappings.highlights.get(capture_name)
        {
            return resolve_user_mapping(mapped);
        }
    }

    // @none is a special nvim-treesitter convention: it resets parent
    // highlighting within a region (e.g., `(interpolation) @none` punches
    // holes in `@string` for f-string interpolation). Recognized here so
    // apply_none_preprocessing() can split parent tokens around @none
    // regions before the sweep line runs.
    if capture_name == "none" {
        return CaptureResult::NoneCapture;
    }

    // No user mapping - resolve the capture name against the built-in legend.
    // Known base types resolve to their indices; unknown ones become Transparent
    // tokens that generate sweep-line breakpoints without competing for winner
    // selection. Resolving here (instead of at encode time) drops the per-token
    // String that previously rode through the whole pipeline.
    match map_capture_to_token_type_and_modifiers(capture_name) {
        Some((token_type, modifiers)) => CaptureResult::Mapped(token_type, modifiers),
        None => CaptureResult::Transparent,
    }
}

/// Resolve a non-empty user-config mapping value to a [`CaptureResult`].
///
/// An empty value means "suppress"; a value whose base type isn't in the legend
/// becomes [`CaptureResult::MappedUnknown`] (preserving the historical
/// compete-then-skip behavior) rather than being silently dropped.
fn resolve_user_mapping(mapped: &str) -> CaptureResult {
    if mapped.is_empty() {
        return CaptureResult::Suppressed;
    }
    match map_capture_to_token_type_and_modifiers(mapped) {
        Some((token_type, modifiers)) => CaptureResult::Mapped(token_type, modifiers),
        None => CaptureResult::MappedUnknown(mapped.to_string()),
    }
}

/// Map a capture name to LSP semantic token type and modifier indices.
///
/// Names follow `type.modifier1.modifier2`. Returns `None` for unknown types
/// (not in LEGEND_TYPES); unknown modifiers are silently ignored.
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

    /// Shorthand for the pre-resolved `CaptureResult::Mapped(type, modifiers)`
    /// of a legend capture name, so tests read in terms of names not indices.
    fn mapped(name: &str) -> CaptureResult {
        let (token_type, modifiers) = map_capture_to_token_type_and_modifiers(name)
            .expect("name must be a valid legend type");
        CaptureResult::Mapped(token_type, modifiers)
    }

    #[test]
    fn test_legend_types_includes_keyword() {
        assert!(LEGEND_TYPES.iter().any(|t| t.as_str() == "keyword"));
    }

    #[test]
    fn test_legend_modifiers_includes_readonly() {
        assert!(LEGEND_MODIFIERS.iter().any(|m| m.as_str() == "readonly"));
    }

    #[test]
    fn resolve_capture_uses_wildcard_merge() {
        // wildcard-config-inheritance: When both wildcard and specific key exist, merge them
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
            mapped("variable"),
            "Should inherit 'variable' mapping from wildcard for 'rust'"
        );

        // Test: "type.builtin" should use rust-specific mapping
        let result = resolve_capture("type.builtin", Some("rust"), Some(&mappings));
        assert_eq!(
            result,
            mapped("type.defaultLibrary"),
            "Should use rust-specific 'type.builtin' mapping"
        );

        // Test: "function" should be inherited from wildcard for "rust"
        let result = resolve_capture("function", Some("rust"), Some(&mappings));
        assert_eq!(
            result,
            mapped("function"),
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
    #[case::known_comment("comment", mapped("comment"))]
    #[case::known_keyword("keyword", mapped("keyword"))]
    #[case::known_variable_with_modifier("variable.readonly", mapped("variable.readonly"))]
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
