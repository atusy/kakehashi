//! Default configuration values for kakehashi.
//!
//! This module provides type-safe default values that are used by `config init`
//! to generate configuration templates.

use super::WILDCARD_KEY;
use super::settings::{
    AggregationConfig, AggregationStrategy, BridgeLanguageConfig, BridgeServerConfig,
    CaptureMapping, CaptureMappings, LanguageSettings, LayerAggregationConfig, LayerSource,
    LayersConfig, QueryTypeMappings, RawWorkspaceSettings, RootMarker,
};
use std::collections::HashMap;

/// Returns the default RawWorkspaceSettings for configuration generation.
///
/// This is used by `config init` to generate type-safe default configurations.
pub fn default_settings() -> RawWorkspaceSettings {
    RawWorkspaceSettings {
        search_paths: Some(vec!["${KAKEHASHI_DATA_DIR}".to_string()]),
        languages: default_languages(),
        capture_mappings: default_capture_mappings(),
        auto_install: Some(true),
        language_servers: Some(default_language_servers()),
    }
}

/// Returns the default languageServers map: a defaults-only `_` wildcard
/// entry documenting the built-in `rootMarkers` default that every concrete
/// server inherits (wildcard-config-inheritance). Not spawnable itself —
/// lookups skip the wildcard key and any server with an empty resolved cmd.
fn default_language_servers() -> HashMap<String, BridgeServerConfig> {
    HashMap::from([(
        WILDCARD_KEY.to_string(),
        BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: None,
            root_markers: Some(vec![RootMarker::Single(".git".to_string())]),
            on_type_formatting_triggers: None,
        },
    )])
}

/// Returns the default languages map containing the wildcard `_` entry.
///
/// The wildcard language is the root of all base chains (base-language-inheritance).
/// It spells out the built-in defaults so the generated template is
/// self-documenting — deleting any entry below never changes behavior:
/// - `layers.aggregation` (cross-layer-aggregation): all three layers in
///   `["virt", "host", "native"]` priority, `Preferred` strategy except for
///   formatting and diagnostics, which combine layers by `Concatenated`.
/// - `bridge` (per-target server aggregation): all bridging enabled, `["*"]`
///   fan-out, `Preferred` strategy except diagnostics (`Concatenated`).
fn default_languages() -> HashMap<String, LanguageSettings> {
    HashMap::from([(
        WILDCARD_KEY.to_string(),
        LanguageSettings {
            base: Some(WILDCARD_KEY.to_string()),
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([
                    (
                        WILDCARD_KEY.to_string(),
                        LayerAggregationConfig {
                            priorities: Some(vec![
                                LayerSource::Virt,
                                LayerSource::Host,
                                LayerSource::Native,
                            ]),
                            strategy: Some(AggregationStrategy::Preferred),
                        },
                    ),
                    (
                        "textDocument/formatting".to_string(),
                        LayerAggregationConfig {
                            priorities: None,
                            strategy: Some(AggregationStrategy::Concatenated),
                        },
                    ),
                    (
                        "textDocument/diagnostic".to_string(),
                        LayerAggregationConfig {
                            priorities: None,
                            strategy: Some(AggregationStrategy::Concatenated),
                        },
                    ),
                    (
                        "textDocument/publishDiagnostics".to_string(),
                        LayerAggregationConfig {
                            priorities: None,
                            strategy: Some(AggregationStrategy::Concatenated),
                        },
                    ),
                ])),
            }),
            bridge: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                BridgeLanguageConfig {
                    enabled: Some(true),
                    aggregation: Some(HashMap::from([
                        (
                            WILDCARD_KEY.to_string(),
                            AggregationConfig {
                                priorities: Some(vec!["*".to_string()]),
                                strategy: Some(AggregationStrategy::Preferred),
                                ..Default::default()
                            },
                        ),
                        (
                            "textDocument/diagnostic".to_string(),
                            AggregationConfig {
                                strategy: Some(AggregationStrategy::Concatenated),
                                ..Default::default()
                            },
                        ),
                        (
                            "textDocument/publishDiagnostics".to_string(),
                            AggregationConfig {
                                strategy: Some(AggregationStrategy::Concatenated),
                                ..Default::default()
                            },
                        ),
                    ])),
                },
            )])),
            ..Default::default()
        },
    )])
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
    use crate::config::settings::AggregationStrategy;

    #[test]
    fn default_settings_has_wildcard_language_with_bridge_defaults() {
        let settings = default_settings();

        // "_" key should exist
        let wildcard = settings
            .languages
            .get(WILDCARD_KEY)
            .expect("should have wildcard '_' language");

        // Self-referential base (chain terminator)
        assert_eq!(wildcard.base.as_deref(), Some(WILDCARD_KEY));

        // Bridge wildcard with enabled: true
        let bridge = wildcard.bridge.as_ref().expect("should have bridge");
        let bridge_wildcard = bridge
            .get(WILDCARD_KEY)
            .expect("should have bridge wildcard '_'");
        assert_eq!(bridge_wildcard.enabled, Some(true));

        // Aggregation strategies
        let agg = bridge_wildcard
            .aggregation
            .as_ref()
            .expect("should have aggregation");

        let wildcard_agg = agg.get(WILDCARD_KEY).expect("should have '_' aggregation");
        assert_eq!(wildcard_agg.strategy, Some(AggregationStrategy::Preferred));

        let diag_agg = agg
            .get("textDocument/diagnostic")
            .expect("should have diagnostic aggregation");
        assert_eq!(diag_agg.strategy, Some(AggregationStrategy::Concatenated));

        let pub_diag_agg = agg
            .get("textDocument/publishDiagnostics")
            .expect("should have publishDiagnostics aggregation");
        assert_eq!(
            pub_diag_agg.strategy,
            Some(AggregationStrategy::Concatenated)
        );
    }

    #[test]
    fn default_settings_documents_builtin_layer_defaults() {
        // The template mirrors the built-in runtime defaults so that the
        // generated file is self-documenting and deleting an entry never
        // changes behavior.
        use crate::config::settings::LayerSource;

        let settings = default_settings();
        let wildcard = settings
            .languages
            .get(WILDCARD_KEY)
            .expect("should have wildcard '_' language");

        let layers = wildcard.layers.as_ref().expect("should have layers");
        let agg = layers
            .aggregation
            .as_ref()
            .expect("should have layers.aggregation");

        let method_wildcard = agg.get(WILDCARD_KEY).expect("should have '_' entry");
        assert_eq!(
            method_wildcard.priorities,
            Some(vec![
                LayerSource::Virt,
                LayerSource::Host,
                LayerSource::Native
            ]),
        );
        assert_eq!(
            method_wildcard.strategy,
            Some(AggregationStrategy::Preferred)
        );

        for method in [
            "textDocument/formatting",
            "textDocument/diagnostic",
            "textDocument/publishDiagnostics",
        ] {
            let entry = agg
                .get(method)
                .unwrap_or_else(|| panic!("should have {method} entry"));
            assert_eq!(
                entry.strategy,
                Some(AggregationStrategy::Concatenated),
                "{method} should document the concatenated default"
            );
        }
    }

    #[test]
    fn default_settings_documents_bridge_priorities_wildcard() {
        let settings = default_settings();
        let bridge_wildcard = settings.languages[WILDCARD_KEY].bridge.as_ref().unwrap()
            [WILDCARD_KEY]
            .aggregation
            .as_ref()
            .unwrap()[WILDCARD_KEY]
            .clone();
        assert_eq!(
            bridge_wildcard.priorities,
            Some(vec!["*".to_string()]),
            "the '*' fan-out default should be visible in the template"
        );
    }

    #[test]
    fn default_settings_documents_root_markers_default() {
        let settings = default_settings();
        let servers = settings
            .language_servers
            .as_ref()
            .expect("should have languageServers");
        let wildcard = servers.get(WILDCARD_KEY).expect("should have '_' entry");
        assert_eq!(
            wildcard.root_markers,
            Some(vec![RootMarker::Single(".git".to_string())])
        );
        assert!(
            wildcard.cmd.is_empty() && wildcard.languages.is_empty(),
            "the wildcard entry is defaults-only, not a spawnable server"
        );

        let toml_string = toml::to_string_pretty(&settings).expect("should serialize");
        assert!(
            toml_string.contains("[languageServers._]"),
            "template should render the wildcard server entry. Got:\n{toml_string}"
        );
        assert!(
            !toml_string.contains("cmd = []"),
            "empty cmd/languages must not clutter the template. Got:\n{toml_string}"
        );
    }

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
