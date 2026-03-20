//! Default configuration values for kakehashi.
//!
//! This module provides type-safe default values that are used by `config init`
//! to generate configuration templates.

use super::WILDCARD_KEY;
use super::settings::{CaptureMapping, CaptureMappings, QueryTypeMappings, RawWorkspaceSettings};
use std::collections::HashMap;

/// Returns the default RawWorkspaceSettings for configuration generation.
///
/// This is used by `config init` to generate type-safe default configurations.
pub fn default_settings() -> RawWorkspaceSettings {
    RawWorkspaceSettings {
        search_paths: Some(vec!["${KAKEHASHI_DATA_DIR}".to_string()]),
        languages: HashMap::new(),
        capture_mappings: default_capture_mappings(),
        auto_install: Some(true),
        language_servers: None,
    }
}

/// Returns the default capture mappings for semantic token translation.
///
/// These mappings translate Tree-sitter capture names (e.g., "variable.builtin")
/// to LSP semantic token types (e.g., "variable.defaultLibrary").
pub fn default_capture_mappings() -> CaptureMappings {
    let mut mappings = CaptureMappings::new();

    let highlights = default_highlight_mappings();
    let wildcard = QueryTypeMappings {
        highlights,
        locals: CaptureMapping::new(),
        folds: CaptureMapping::new(),
    };

    mappings.insert(WILDCARD_KEY.to_string(), wildcard);
    mappings
}

/// Returns the default highlight capture mappings.
fn default_highlight_mappings() -> CaptureMapping {
    let pairs = [
        // Variables
        ("variable", "variable"),
        ("variable.builtin", "variable.defaultLibrary"),
        ("variable.parameter", "parameter"),
        ("variable.parameter.builtin", "parameter.defaultLibrary"),
        ("variable.member", "property"),
        // Constants
        ("constant", "variable.readonly"),
        ("constant.builtin", "variable.readonly.defaultLibrary"),
        ("constant.macro", "macro"),
        // Modules
        ("module", "namespace"),
        ("module.builtin", "namespace.defaultLibrary"),
        ("label", "variable"),
        // Strings
        ("string", "string"),
        ("string.documentation", "string.documentation"),
        ("string.regexp", "regexp"),
        ("string.escape", "string"),
        ("string.special", "string"),
        ("string.special.symbol", "string"),
        ("string.special.path", "string"),
        ("string.special.url", "string"),
        ("character", "string"),
        ("character.special", "string"),
        // Literals
        ("boolean", "keyword"),
        ("number", "number"),
        ("number.float", "number"),
        // Types
        ("type", "type"),
        ("type.builtin", "type.defaultLibrary"),
        ("type.definition", "type.definition"),
        // Attributes
        ("attribute", "decorator"),
        ("attribute.builtin", "decorator.defaultLibrary"),
        ("property", "property"),
        // Functions
        ("function", "function"),
        ("function.builtin", "function.defaultLibrary"),
        ("function.call", "function"),
        ("function.macro", "macro"),
        ("function.method", "method"),
        ("function.method.call", "method"),
        ("constructor", "function"),
        // Operators
        ("operator", "operator"),
        // Keywords
        ("keyword", "keyword"),
        ("keyword.coroutine", "keyword.async"),
        ("keyword.function", "keyword"),
        ("keyword.operator", "operator"),
        ("keyword.import", "keyword"),
        ("keyword.type", "keyword"),
        ("keyword.modifier", "modifier"),
        ("keyword.repeat", "keyword"),
        ("keyword.return", "keyword"),
        ("keyword.debug", "keyword"),
        ("keyword.exception", "keyword"),
        ("keyword.conditional", "keyword"),
        ("keyword.conditional.ternary", "operator"),
        ("keyword.directive", "macro"),
        ("keyword.directive.define", "macro"),
        // Punctuation (map to empty string to suppress)
        ("punctuation.delimiter", ""),
        ("punctuation.bracket", ""),
        ("punctuation.special", ""),
        // Comments
        ("comment", "comment"),
        ("comment.documentation", "comment.documentation"),
        ("comment.error", "comment"),
        ("comment.warning", "comment"),
        ("comment.todo", "comment"),
        ("comment.note", "comment"),
        // Markup (most map to empty to suppress)
        ("markup.strong", ""),
        ("markup.italic", ""),
        ("markup.strikethrough", ""),
        ("markup.underline", ""),
        ("markup.heading", ""),
        ("markup.heading.1", ""),
        ("markup.heading.2", ""),
        ("markup.heading.3", ""),
        ("markup.heading.4", ""),
        ("markup.heading.5", ""),
        ("markup.heading.6", ""),
        ("markup.quote", ""),
        ("markup.math", ""),
        ("markup.link", ""),
        ("markup.link.label", ""),
        ("markup.link.url", ""),
        ("markup.raw", "string"),
        ("markup.raw.block", "string"),
        ("markup.list", ""),
        ("markup.list.checked", ""),
        ("markup.list.unchecked", ""),
        // Diff
        ("diff.plus", ""),
        ("diff.minus", ""),
        ("diff.delta", ""),
        // Tags (XML/HTML)
        ("tag", "class"),
        ("tag.builtin", "class.defaultLibrary"),
        ("tag.attribute", "property"),
        ("tag.delimiter", ""),
    ];

    pairs
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capture_mappings_contains_variable_mapping() {
        let mappings = default_capture_mappings();

        // The wildcard "_" key should exist with highlights mappings
        let wildcard = mappings
            .get(WILDCARD_KEY)
            .expect("should have wildcard '_' key");

        // "variable" should map to "variable" (identity mapping)
        assert_eq!(
            wildcard.highlights.get("variable"),
            Some(&"variable".to_string()),
            "should map 'variable' capture to 'variable' token type"
        );
    }

    #[test]
    fn default_settings_has_auto_install_true() {
        let settings = default_settings();

        // autoInstall should default to true for zero-config experience
        assert_eq!(
            settings.auto_install,
            Some(true),
            "autoInstall should be Some(true) by default"
        );
    }

    #[test]
    fn default_settings_has_capture_mappings() {
        let settings = default_settings();

        // Should have capture mappings populated
        assert!(
            !settings.capture_mappings.is_empty(),
            "capture_mappings should not be empty"
        );

        // Should contain the wildcard "_" key
        assert!(
            settings.capture_mappings.contains_key(WILDCARD_KEY),
            "capture_mappings should contain wildcard '_' key"
        );
    }

    #[test]
    fn default_settings_serializes_to_valid_toml() {
        let settings = default_settings();

        // Should serialize to valid TOML
        let toml_string =
            toml::to_string_pretty(&settings).expect("should serialize to TOML without error");

        // Should contain autoInstall setting
        assert!(
            toml_string.contains("autoInstall = true"),
            "TOML should contain 'autoInstall = true'. Got:\n{}",
            toml_string
        );

        // Should contain captureMappings section
        assert!(
            toml_string.contains("[captureMappings._.highlights]"),
            "TOML should contain captureMappings section. Got:\n{}",
            toml_string
        );

        // Should contain at least one mapping (variable)
        assert!(
            toml_string.contains("\"variable\""),
            "TOML should contain variable mapping. Got:\n{}",
            toml_string
        );
    }

    #[test]
    fn default_settings_has_search_paths_template() {
        let settings = default_settings();
        assert_eq!(
            settings.search_paths,
            Some(vec!["${KAKEHASHI_DATA_DIR}".to_string()]),
            "searchPaths should default to [\"${{KAKEHASHI_DATA_DIR}}\"]"
        );
    }

    #[test]
    fn default_settings_search_paths_in_toml() {
        let settings = default_settings();
        let toml_string =
            toml::to_string_pretty(&settings).expect("should serialize to TOML without error");
        assert!(
            toml_string.contains("searchPaths"),
            "TOML should contain searchPaths. Got:\n{}",
            toml_string
        );
        assert!(
            toml_string.contains("KAKEHASHI_DATA_DIR"),
            "TOML should contain KAKEHASHI_DATA_DIR template. Got:\n{}",
            toml_string
        );
    }

    #[test]
    fn default_settings_through_coordinator_has_markup_strong_mapping() {
        // This test verifies the full chain from default_settings() through
        // WorkspaceSettings and into LanguageCoordinator, ensuring that
        // markup.strong -> "" mapping is preserved.
        use crate::config::WorkspaceSettings;
        use crate::language::LanguageCoordinator;

        // Create settings from defaults — use with_kakehashi_defaults so that
        // ${KAKEHASHI_DATA_DIR} in searchPaths resolves to the platform default.
        use crate::config::expand::with_kakehashi_defaults;
        let raw_settings = default_settings();
        let ws_settings = WorkspaceSettings::try_from_settings(
            &raw_settings,
            None,
            with_kakehashi_defaults(|_| None),
        )
        .expect("default settings should expand without errors");

        // Verify WorkspaceSettings has the mapping
        assert!(
            ws_settings.capture_mappings.contains_key(WILDCARD_KEY),
            "WorkspaceSettings should have wildcard key"
        );
        let wildcard = ws_settings.capture_mappings.get(WILDCARD_KEY).unwrap();
        assert_eq!(
            wildcard.highlights.get("markup.strong"),
            Some(&String::new()),
            "WorkspaceSettings should map markup.strong to empty string"
        );

        // Load into coordinator
        let coordinator = LanguageCoordinator::new();
        let _summary = coordinator.load_settings(&ws_settings);

        // Verify coordinator has the mapping
        let mappings = coordinator.capture_mappings();
        assert!(
            mappings.contains_key(WILDCARD_KEY),
            "Coordinator should have wildcard key after loading settings"
        );
        let coord_wildcard = mappings.get(WILDCARD_KEY).unwrap();
        assert_eq!(
            coord_wildcard.highlights.get("markup.strong"),
            Some(&String::new()),
            "Coordinator should map markup.strong to empty string after loading settings"
        );
    }
}
