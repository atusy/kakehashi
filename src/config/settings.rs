use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use super::WILDCARD_KEY;

pub(crate) type CaptureMapping = HashMap<String, String>;

/// Aggregation strategy for combining results from multiple bridge servers.
///
/// - `Preferred`: Use the first non-empty response (priority-ordered).
///   This is the default for most LSP methods.
/// - `Concatenated`: Collect and merge responses from all servers.
///   This is the default for `textDocument/diagnostic`.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AggregationStrategy {
    Preferred,
    Concatenated,
}

/// Per-method aggregation configuration.
///
/// Controls how results from multiple bridge servers are aggregated for a
/// specific LSP method. The `priorities` list determines server preference
/// order — the first server in the list that returns a non-empty result wins.
/// Empty priorities degrades to first-win (arrival-order) behavior.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AggregationConfig {
    /// Server names in priority order (highest first). Empty = pure first-win behavior.
    #[serde(default)]
    pub priorities: Vec<String>,
    /// Aggregation strategy override. Omit to use the handler default.
    #[serde(default)]
    pub strategy: Option<AggregationStrategy>,
    /// Maximum number of servers to fan out to.
    ///
    /// - `None` / absent: no limit (fan out to all matching servers)
    /// - `0`: disable fan-out entirely
    /// - Positive: cap the number of concurrent server requests
    /// - Negative: treated as no limit (silently ignored via `usize::try_from`)
    #[serde(default)]
    pub max_fan_out: Option<i64>,
}

/// Configuration for a single bridged language within a host filetype.
///
/// Used in the bridge filter map to control whether a specific injection language
/// should be bridged and how results are aggregated.
/// Example: `{ python = { enabled = true, aggregation = { "_" = { priorities = ["pyright"] } } } }`.
///
/// Omitted fields inherit from the wildcard `_` entry (see ADR-0011).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default, JsonSchema)]
pub struct BridgeLanguageConfig {
    /// Whether bridging is enabled for this language.
    /// Omit to inherit from wildcard (defaults to true).
    pub enabled: Option<bool>,
    /// Per-method aggregation config. Key = LSP method name or "_" for default.
    pub aggregation: Option<HashMap<String, AggregationConfig>>,
}

/// Fully resolved aggregation settings for a single LSP method.
///
/// Produced by [`BridgeLanguageConfig::resolve_aggregation`]. Unlike
/// [`AggregationConfig`] (the raw TOML struct), all optional fields are
/// resolved with their defaults.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedAggregationConfig {
    pub(crate) strategy: AggregationStrategy,
    pub(crate) priorities: Vec<String>,
    pub(crate) max_fan_out: Option<usize>,
}

impl ResolvedAggregationConfig {
    /// Create a config with the given default strategy and all other fields at their defaults.
    pub(crate) fn with_defaults(default_strategy: AggregationStrategy) -> Self {
        Self {
            strategy: default_strategy,
            priorities: Vec::new(),
            max_fan_out: None,
        }
    }
}

impl BridgeLanguageConfig {
    /// Look up the aggregation entry for a method, falling back to wildcard `"_"`.
    fn resolve_aggregation_entry(&self, method: &str) -> Option<&AggregationConfig> {
        let map = self.aggregation.as_ref()?;
        map.get(method).or_else(|| map.get(WILDCARD_KEY))
    }

    /// Resolve all aggregation settings for a specific LSP method in a single call.
    ///
    /// Performs one config lookup and extracts strategy, priorities, and max_fan_out
    /// together, avoiding redundant resolution when all three are needed.
    pub(crate) fn resolve_aggregation(
        &self,
        method: &str,
        default_strategy: AggregationStrategy,
    ) -> ResolvedAggregationConfig {
        match self.resolve_aggregation_entry(method) {
            Some(entry) => ResolvedAggregationConfig {
                strategy: entry.strategy.unwrap_or(default_strategy),
                priorities: entry.priorities.clone(),
                max_fan_out: entry.max_fan_out.and_then(|raw| usize::try_from(raw).ok()),
            },
            None => ResolvedAggregationConfig::with_defaults(default_strategy),
        }
    }
}

/// Configuration for a bridge language server.
///
/// This is used to configure external language servers (like rust-analyzer, pyright)
/// that kakehashi can redirect requests to for injection regions.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BridgeServerConfig {
    /// Command array: first element is the program, rest are arguments
    /// e.g., ["rust-analyzer"] or ["pyright-langserver", "--stdio"]
    pub cmd: Vec<String>,
    /// Languages this server handles (e.g., ["rust"], ["python"])
    pub languages: Vec<String>,
    /// Optional initialization options to pass to the server during initialize
    pub initialization_options: Option<Value>,
}

/// Custom mappings from Tree-sitter capture names to semantic token types, per query kind.
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq, JsonSchema)]
pub struct QueryTypeMappings {
    /// Capture mappings for highlights queries.
    #[serde(default)]
    pub highlights: CaptureMapping,
    /// Capture mappings for locals queries.
    #[serde(default)]
    pub locals: CaptureMapping,
    /// Capture mappings for folds queries.
    /// Reserved for future folding range support — populated and merged but not yet consumed by analysis.
    #[serde(default)]
    pub folds: CaptureMapping,
}

pub type CaptureMappings = HashMap<String, QueryTypeMappings>;

/// Query type for tree-sitter query files.
///
/// Used in the unified `queries` field to specify what kind of query a file contains.
/// When not specified, the kind is inferred from the filename pattern.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum QueryKind {
    /// Syntax highlighting queries
    Highlights,
    /// Local definitions/references queries (for scope analysis)
    Locals,
    /// Language injection queries (for embedded languages)
    Injections,
}

/// A single query file configuration entry.
///
/// Used in the unified `queries` array to specify query files with optional type.
/// Example: `{ path = "./highlights.scm", kind = "highlights" }`
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema)]
pub struct QueryItem {
    /// Path to the query file (required)
    pub path: String,
    /// Query type: highlights, locals, or injections (optional - inferred from filename if omitted)
    pub kind: Option<QueryKind>,
}

/// Infer the query kind from a file path based on filename patterns.
///
/// Rules:
/// - Exact match `highlights.scm` -> `Some(Highlights)`
/// - Exact match `locals.scm` -> `Some(Locals)`
/// - Exact match `injections.scm` -> `Some(Injections)`
/// - Otherwise -> `None`
///
/// Examples:
/// - `injections.scm` -> matches
/// - `rust-injections.scm` -> does NOT match (only exact filename matches)
/// - `local-injections.scm` -> does NOT match (only exact filename matches)
pub(crate) fn infer_query_kind(path: &str) -> Option<QueryKind> {
    // Extract filename from path using std::path for cross-platform support
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path);

    // Only match exact filenames
    match filename {
        "injections.scm" => Some(QueryKind::Injections),
        "locals.scm" => Some(QueryKind::Locals),
        "highlights.scm" => Some(QueryKind::Highlights),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RawWorkspaceSettings {
    /// Directories to search for Tree-sitter parser libraries and query files.
    pub search_paths: Option<Vec<String>>,
    /// Per-language configuration (parser paths, queries, bridge filters, base inheritance).
    #[serde(default)]
    pub languages: HashMap<String, LanguageSettings>,
    /// Custom mappings from Tree-sitter capture names to semantic token types.
    #[serde(default)]
    pub capture_mappings: CaptureMappings,
    /// Whether to automatically install missing parsers and queries.
    pub auto_install: Option<bool>,
    /// Language servers for bridging LSP requests to injection regions.
    /// Map of server name to server configuration.
    pub language_servers: Option<HashMap<String, BridgeServerConfig>>,
}

/// Generate JSON Schema for the workspace configuration.
pub fn json_schema() -> schemars::Schema {
    schemars::schema_for!(RawWorkspaceSettings)
}

// Domain types - used throughout the application and also exposed in JSON Schema
// via RawWorkspaceSettings.

/// Per-language Tree-sitter configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct LanguageSettings {
    /// Base language to inherit parser, queries, and bridge config from.
    /// When set, the derived language uses the base's config entirely.
    /// E.g., `base = "markdown"` on `rmd` means rmd uses markdown's config.
    pub base: Option<String>,
    /// Path to the parser library (.so/.dylib/.dll)
    pub parser: Option<String>,
    /// Omit to inherit from wildcard/defaults. Use an empty array `[]` to explicitly clear queries.
    pub queries: Option<Vec<QueryItem>>,
    /// Omit to bridge all configured languages (default). Use an empty object `{}` to disable bridging. Use `{ "python": { "enabled": true } }` to bridge specific languages.
    pub bridge: Option<HashMap<String, BridgeLanguageConfig>>,
    /// Deprecated: use `base` on the derived language instead.
    /// Alternative languageId values that should use this parser.
    pub aliases: Option<Vec<String>>,
}

impl LanguageSettings {
    /// Check if a language is allowed for bridging based on the bridge filter.
    ///
    /// Returns:
    /// - `true` if `bridge` is `None` (default: bridge all languages)
    /// - `false` if `bridge` is `Some({})` (empty map: bridge nothing)
    /// - For non-empty maps: resolves wildcard `_` with specific key, then checks `enabled`
    ///   - `enabled: None` → defaults to `true` (bridging enabled by default)
    ///   - `enabled: Some(v)` → uses explicit value
    ///   - No wildcard and no specific key → `false` (not in map)
    pub fn is_language_bridgeable(&self, injection_language: &str) -> bool {
        match &self.bridge {
            None => true,                         // Default: bridge all configured languages
            Some(map) if map.is_empty() => false, // Empty map: bridge nothing
            Some(map) => {
                let resolved = crate::config::resolve_with_wildcard(
                    map,
                    injection_language,
                    crate::config::merge_bridge_language_configs,
                );
                resolved.map(|c| c.enabled.unwrap_or(true)).unwrap_or(false)
            }
        }
    }
}

/// Workspace-wide Tree-sitter configuration as required by the domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceSettings {
    pub search_paths: Vec<String>,
    pub languages: HashMap<String, LanguageSettings>,
    pub capture_mappings: CaptureMappings,
    pub auto_install: bool,
    pub language_servers: HashMap<String, BridgeServerConfig>,
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self {
            search_paths: Vec::new(),
            languages: HashMap::new(),
            capture_mappings: CaptureMappings::default(),
            auto_install: true, // Default to true for zero-config experience
            language_servers: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn test_json_schema_generation() {
        let schema = schemars::schema_for!(RawWorkspaceSettings);
        let json = serde_json::to_string_pretty(&schema).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // Top-level properties should use camelCase (from serde renames)
        let props = value.get("properties").expect("should have properties");
        assert!(props.get("searchPaths").is_some(), "missing searchPaths");
        assert!(props.get("autoInstall").is_some(), "missing autoInstall");
        assert!(
            props.get("captureMappings").is_some(),
            "missing captureMappings"
        );
        assert!(
            props.get("languageServers").is_some(),
            "missing languageServers"
        );

        // snake_case variants must NOT appear
        assert!(
            props.get("search_paths").is_none(),
            "snake_case leak: search_paths"
        );
        assert!(
            props.get("auto_install").is_none(),
            "snake_case leak: auto_install"
        );
        assert!(
            props.get("capture_mappings").is_none(),
            "snake_case leak: capture_mappings"
        );
        assert!(
            props.get("language_servers").is_none(),
            "snake_case leak: language_servers"
        );

        // $defs should contain key referenced types
        let defs = value.get("$defs").expect("should have $defs");
        for expected in [
            "LanguageSettings",
            "BridgeServerConfig",
            "BridgeLanguageConfig",
            "QueryItem",
            "AggregationConfig",
        ] {
            assert!(
                defs.get(expected).is_some(),
                "missing $defs entry: {expected}"
            );
        }
    }

    #[test]
    fn should_distinguish_between_unspecified_and_empty_queries() {
        // This test demonstrates the need for Option<Vec<QueryItem>>
        // to distinguish between "not specified" and "explicitly empty"
        // which is critical for merging logic in resolve_with_wildcard + merge_language_settings

        // Case 1: queries not specified (should be None)
        // User didn't specify queries - should inherit from wildcard/defaults
        let unspecified = LanguageSettings::default();
        assert!(
            unspecified.queries.is_none(),
            "Unspecified queries should be None"
        );

        // Case 2: queries explicitly empty (should be Some([]))
        // User explicitly set queries to empty - should override wildcard with empty list
        let explicitly_empty = LanguageSettings {
            queries: Some(vec![]),
            ..Default::default()
        };
        assert!(
            explicitly_empty.queries.is_some(),
            "Explicitly empty should be Some"
        );
        assert!(
            explicitly_empty.queries.as_ref().unwrap().is_empty(),
            "Should be empty vec"
        );

        // Case 3: queries with items
        let with_items = LanguageSettings {
            queries: Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };
        assert!(with_items.queries.is_some());
        assert_eq!(with_items.queries.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn should_parse_valid_configuration_with_queries() {
        // Configuration using unified queries field
        let config_json = r#"{
            "languages": {
                "rust": {
                    "parser": "/path/to/rust.so",
                    "queries": [
                        {"path": "/path/to/highlights.scm", "kind": "highlights"},
                        {"path": "/path/to/custom.scm"}
                    ]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.search_paths.is_none());
        assert!(settings.languages.contains_key("rust"));
        assert_eq!(
            settings.languages["rust"].parser,
            Some("/path/to/rust.so".to_string())
        );

        let queries = settings.languages["rust"].queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].path, "/path/to/highlights.scm");
        assert_eq!(queries[0].kind, Some(QueryKind::Highlights));
        assert_eq!(queries[1].path, "/path/to/custom.scm");
        assert_eq!(queries[1].kind, None); // Kind not specified - will be inferred later
    }

    #[rstest]
    #[case::locals("locals", QueryKind::Locals)]
    #[case::injections("injections", QueryKind::Injections)]
    fn should_parse_configuration_with_query_kind(
        #[case] kind_str: &str,
        #[case] expected: QueryKind,
    ) {
        let config_json = format!(
            r#"{{
            "languages": {{
                "rust": {{
                    "queries": [
                        {{"path": "/path/to/highlights.scm", "kind": "highlights"}},
                        {{"path": "/path/to/{kind_str}.scm", "kind": "{kind_str}"}}
                    ]
                }}
            }}
        }}"#
        );

        let settings: RawWorkspaceSettings = serde_json::from_str(&config_json).unwrap();
        let queries = settings.languages["rust"].queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[1].path, format!("/path/to/{kind_str}.scm"));
        assert_eq!(queries[1].kind, Some(expected));
    }

    #[test]
    fn should_reject_invalid_json() {
        let invalid_json = r#"{
            "treesitter": {
                "rust": {
                    "parser": "/path/to/rust.so"
                    // Missing comma - invalid JSON
                    "highlight": []
                }
            }
        }"#;

        let result = serde_json::from_str::<RawWorkspaceSettings>(invalid_json);
        assert!(result.is_err());
    }

    #[test]
    fn should_handle_empty_configurations() {
        let empty_json = r#"{
            "languages": {}
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(empty_json).unwrap();
        assert!(settings.languages.is_empty());
    }

    #[test]
    fn should_handle_completely_empty_json_object() {
        // This is crucial for zero-config: InitializationOptions = {} should work
        let completely_empty = r#"{}"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(completely_empty).unwrap();
        assert!(settings.languages.is_empty());
        assert!(settings.search_paths.is_none());
        assert!(settings.auto_install.is_none());
        assert!(settings.capture_mappings.is_empty());
    }

    #[test]
    fn should_handle_missing_languages_field() {
        let json_without_languages = r#"{
            "searchPaths": ["/some/path"],
            "captureMappings": {
                "_": {
                    "highlights": {}
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(json_without_languages).unwrap();
        assert!(settings.languages.is_empty());
        assert_eq!(settings.search_paths, Some(vec!["/some/path".to_string()]));
    }

    #[test]
    fn should_parse_toml_without_languages_field() {
        let toml_without_languages = r#"
            [captureMappings._.highlights]
        "#;

        let settings: RawWorkspaceSettings = toml::from_str(toml_without_languages).unwrap();
        assert!(settings.languages.is_empty());
    }

    #[rstest]
    #[case::basic(
        r#"{"searchPaths": ["/usr/local/lib/tree-sitter", "/opt/tree-sitter/parsers"],
            "languages": {"rust": {"queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]}}}"#,
        &["/usr/local/lib/tree-sitter", "/opt/tree-sitter/parsers"],
    )]
    #[case::different_paths(
        r#"{"searchPaths": ["/data/tree-sitter", "/assets/ts"],
            "languages": {"lua": {"queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]}}}"#,
        &["/data/tree-sitter", "/assets/ts"],
    )]
    fn should_parse_searchpaths_configuration(
        #[case] config_json: &str,
        #[case] expected_paths: &[&str],
    ) {
        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        assert_eq!(
            settings.search_paths.unwrap(),
            expected_paths
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn should_parse_mixed_configuration_with_searchpaths_and_explicit_parser() {
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib/tree-sitter"],
            "languages": {
                "rust": {
                    "parser": "/custom/path/rust.so",
                    "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
                },
                "python": {
                    "queries": [{"path": "/path/to/python.scm", "kind": "highlights"}]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.search_paths.is_some());
        assert_eq!(
            settings.search_paths.unwrap(),
            vec!["/usr/local/lib/tree-sitter"]
        );

        // rust has explicit parser path
        assert_eq!(
            settings.languages["rust"].parser,
            Some("/custom/path/rust.so".to_string())
        );

        // python will use searchPaths
        assert_eq!(settings.languages["python"].parser, None);
    }

    #[test]
    fn should_handle_malformed_json_gracefully() {
        let malformed_configs = vec![
            r#"{"languages": {"rust": {"parser": "/path"}"#, // Missing closing braces
            r#"{"languages": {"rust": {"parser": "/path", "queries": [}}"#, // Invalid array
        ];

        for config in malformed_configs {
            let result = serde_json::from_str::<RawWorkspaceSettings>(config);
            assert!(result.is_err());
        }
    }

    #[test]
    fn should_parse_capture_mappings() {
        let config_json = r#"{
            "languages": {
                "rust": {
                    "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
                }
            },
            "captureMappings": {
                "_": {
                    "highlights": {
                        "variable.builtin": "variable.defaultLibrary",
                        "function.builtin": "function.defaultLibrary"
                    }
                },
                "rust": {
                    "highlights": {
                        "type.builtin": "type.defaultLibrary"
                    },
                    "locals": {
                        "definition.var": "definition.variable"
                    }
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(settings.capture_mappings);
        });
    }

    #[test]
    fn should_parse_auto_install_setting() {
        let config_json = r#"{
            "autoInstall": true,
            "languages": {}
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        assert_eq!(settings.auto_install, Some(true));

        // Test with false
        let config_false = r#"{
            "autoInstall": false,
            "languages": {}
        }"#;
        let settings_false: RawWorkspaceSettings = serde_json::from_str(config_false).unwrap();
        assert_eq!(settings_false.auto_install, Some(false));

        // Test missing (should be None)
        let config_missing = r#"{
            "languages": {}
        }"#;
        let settings_missing: RawWorkspaceSettings = serde_json::from_str(config_missing).unwrap();
        assert_eq!(settings_missing.auto_install, None);
    }

    #[test]
    fn should_handle_complex_configurations_efficiently() {
        let mut config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        // Add multiple language configurations
        let languages = vec!["rust", "python", "javascript", "typescript", "go"];

        for lang in languages {
            config.languages.insert(
                lang.to_string(),
                LanguageSettings {
                    parser: Some(format!("/usr/lib/libtree-sitter-{}.so", lang)),
                    queries: Some(vec![QueryItem {
                        path: format!("/etc/tree-sitter/{}/highlights.scm", lang),
                        kind: Some(QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            );
        }

        assert_eq!(config.languages.len(), 5);

        // Verify serialization/deserialization still works
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: RawWorkspaceSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.languages.len(), config.languages.len());
    }

    #[test]
    fn should_not_have_legacy_fields_in_language_config() {
        // Legacy highlights/locals/injections fields have been removed from LanguageSettings
        // All query configuration now uses the unified queries field
        let config = LanguageSettings {
            parser: Some("/path/to/parser.so".to_string()),
            queries: Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };

        // Serialize and verify no legacy fields in output
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("\"highlights\":"),
            "LanguageSettings should not have highlights field, but found in JSON: {}",
            json
        );
        assert!(
            !json.contains("\"locals\":"),
            "LanguageSettings should not have locals field, but found in JSON: {}",
            json
        );
        assert!(
            !json.contains("\"injections\":"),
            "LanguageSettings should not have injections field, but found in JSON: {}",
            json
        );
    }

    #[test]
    fn should_create_language_settings_with_parser_and_queries() {
        // PBI-156: LanguageSettings uses parser (not library) and unified queries
        let settings = LanguageSettings {
            parser: Some("/path/to/parser.so".to_string()),
            queries: Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
            ..Default::default()
        };

        assert_eq!(settings.parser, Some("/path/to/parser.so".to_string()));
        assert_eq!(settings.queries.as_ref().unwrap().len(), 1);
        assert_eq!(
            settings.queries.as_ref().unwrap()[0].path,
            "/path/to/highlights.scm"
        );
        assert_eq!(
            settings.queries.as_ref().unwrap()[0].kind,
            Some(QueryKind::Highlights)
        );
    }

    #[test]
    fn should_parse_bridge_server_config() {
        // Test that BridgeServerConfig can deserialize all fields:
        // cmd (required), languages (required), initialization_options (optional)
        let config_json = r#"{
            "cmd": ["rust-analyzer", "--log-file", "/tmp/ra.log"],
            "languages": ["rust"],
            "initializationOptions": {
                "linkedProjects": ["/path/to/Cargo.toml"]
            }
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();

        assert_eq!(
            config.cmd,
            vec![
                "rust-analyzer".to_string(),
                "--log-file".to_string(),
                "/tmp/ra.log".to_string()
            ]
        );
        assert_eq!(config.languages, vec!["rust".to_string()]);
        assert!(config.initialization_options.is_some());
        let init_opts = config.initialization_options.unwrap();
        assert!(init_opts.get("linkedProjects").is_some());
    }

    #[test]
    fn should_parse_bridge_server_config_minimal() {
        // Test that only required fields need to be present
        let config_json = r#"{
            "cmd": ["pyright"],
            "languages": ["python"]
        }"#;

        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();

        assert_eq!(config.cmd, vec!["pyright".to_string()]);
        assert_eq!(config.languages, vec!["python".to_string()]);
        assert!(config.initialization_options.is_none());
    }

    #[test]
    fn should_parse_language_config_with_bridge_map_enabled() {
        // PBI-120: LanguageSettings should parse bridge field as HashMap<String, BridgeLanguageConfig>
        // Example: bridge = { python = { enabled = true }, r = { enabled = true } }
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {
                "python": { "enabled": true },
                "r": { "enabled": true }
            }
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(config.bridge.is_some(), "bridge field should be Some");
        let bridge = config.bridge.unwrap();
        assert_eq!(bridge.len(), 2);
        assert_eq!(bridge.get("python").unwrap().enabled, Some(true));
        assert_eq!(bridge.get("r").unwrap().enabled, Some(true));
    }

    #[test]
    fn should_parse_language_config_with_empty_bridge_map() {
        // PBI-120: Empty bridge map should disable all bridging
        // bridge: {} disables all bridging for that host filetype
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {}
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(
            config.bridge.is_some(),
            "bridge field should be Some(empty map)"
        );
        let bridge = config.bridge.unwrap();
        assert!(bridge.is_empty(), "bridge map should be empty");
    }

    #[test]
    fn should_parse_language_config_without_bridge_field() {
        // PBI-108: Omitted bridge field should be None (bridges all languages)
        // AC3: bridge omitted or null bridges all configured languages
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(
            config.bridge.is_none(),
            "omitted bridge field should be None"
        );
    }

    /// Bridge filter: None (default) bridges all, empty map bridges nothing,
    /// explicit entries control per-language bridging.
    #[rstest]
    // None bridge → all languages bridgeable
    #[case::null_allows_all(None, "python", true)]
    #[case::null_allows_any(None, "any_language", true)]
    // Empty map → nothing bridgeable
    #[case::empty_blocks_all(Some(HashMap::new()), "python", false)]
    #[case::empty_blocks_rust(Some(HashMap::new()), "rust", false)]
    // Explicit enabled entries
    #[case::enabled_language(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
        ])),
        "python",
        true
    )]
    #[case::unlisted_language(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
        ])),
        "rust",
        false
    )]
    #[case::disabled_language(
        Some(HashMap::from([
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "r",
        false
    )]
    // Multi-entry map: enabled + disabled coexist
    #[case::multi_entry_enabled(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "python",
        true
    )]
    #[case::multi_entry_disabled(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "r",
        false
    )]
    #[case::multi_entry_unlisted(
        Some(HashMap::from([
            ("python".to_string(), BridgeLanguageConfig { enabled: Some(true), ..Default::default() }),
            ("r".to_string(), BridgeLanguageConfig { enabled: Some(false), ..Default::default() }),
        ])),
        "rust",
        false
    )]
    fn test_bridge_filter(
        #[case] bridge: Option<HashMap<String, BridgeLanguageConfig>>,
        #[case] language: &str,
        #[case] expected: bool,
    ) {
        let settings = LanguageSettings {
            bridge,
            ..Default::default()
        };
        assert_eq!(
            settings.is_language_bridgeable(language),
            expected,
            "is_language_bridgeable({language:?}) should be {expected}"
        );
    }

    #[test]
    fn should_parse_language_servers_at_root() {
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib"],
            "languageServers": {
                "rust-analyzer": {
                    "cmd": ["rust-analyzer"],
                    "languages": ["rust"]
                },
                "pyright": {
                    "cmd": ["pyright-langserver", "--stdio"],
                    "languages": ["python"]
                }
            }
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();
        assert!(
            settings.language_servers.is_some(),
            "languageServers should be parsed as Some"
        );
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(settings.language_servers);
        });
    }

    #[test]
    fn should_parse_language_servers_empty() {
        // PBI-119: Empty languageServers should be valid
        let config_json = r#"{
            "languageServers": {}
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.language_servers.is_some());
        assert!(settings.language_servers.as_ref().unwrap().is_empty());
    }

    #[test]
    fn should_parse_without_language_servers() {
        // PBI-119: Missing languageServers should be None (backward compatibility)
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib"]
        }"#;

        let settings: RawWorkspaceSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.language_servers.is_none());
    }

    #[test]
    fn should_parse_bridge_language_config() {
        // PBI-120: BridgeLanguageConfig should deserialize with enabled field
        // Example: bridge = { python = { enabled = true } }
        let config_json = r#"{
            "enabled": true
        }"#;

        let config: BridgeLanguageConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(config.enabled, Some(true));

        // Test disabled
        let config_false_json = r#"{
            "enabled": false
        }"#;
        let config_false: BridgeLanguageConfig = serde_json::from_str(config_false_json).unwrap();
        assert_eq!(config_false.enabled, Some(false));
    }

    #[test]
    fn should_parse_language_config_with_bridge_map() {
        // PBI-120: LanguageSettings.bridge should be HashMap<String, BridgeLanguageConfig>
        // Example: bridge = { python = { enabled = true }, r = { enabled = false } }
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {
                "python": { "enabled": true },
                "r": { "enabled": false }
            }
        }"#;

        let config: LanguageSettings = serde_json::from_str(config_json).unwrap();

        assert!(config.bridge.is_some(), "bridge field should be Some");
        let bridge = config.bridge.unwrap();
        assert_eq!(bridge.len(), 2);
        assert_eq!(bridge.get("python").unwrap().enabled, Some(true));
        assert_eq!(bridge.get("r").unwrap().enabled, Some(false));
    }

    // PBI-151: Unified query configuration with QueryItem struct
    #[test]
    fn should_parse_query_item_with_path_and_kind() {
        // QueryItem should have path (required) and kind (optional) fields
        // kind can be "highlights", "locals", or "injections"
        let toml_str = r#"
            path = "/path/to/highlights.scm"
            kind = "highlights"
        "#;

        let item: QueryItem = toml::from_str(toml_str).unwrap();
        assert_eq!(item.path, "/path/to/highlights.scm");
        assert_eq!(item.kind, Some(QueryKind::Highlights));
    }

    #[test]
    fn should_parse_query_item_without_kind() {
        // kind is optional - defaults to None (type inference happens later)
        let toml_str = r#"
            path = "/path/to/custom.scm"
        "#;

        let item: QueryItem = toml::from_str(toml_str).unwrap();
        assert_eq!(item.path, "/path/to/custom.scm");
        assert!(item.kind.is_none());
    }

    #[test]
    fn should_parse_query_kind_enum_variants() {
        // QueryKind enum should have Highlights, Locals, Injections variants
        let highlights_toml = r#"path = "/a.scm"
kind = "highlights""#;
        let locals_toml = r#"path = "/b.scm"
kind = "locals""#;
        let injections_toml = r#"path = "/c.scm"
kind = "injections""#;

        let h: QueryItem = toml::from_str(highlights_toml).unwrap();
        let l: QueryItem = toml::from_str(locals_toml).unwrap();
        let i: QueryItem = toml::from_str(injections_toml).unwrap();

        assert_eq!(h.kind, Some(QueryKind::Highlights));
        assert_eq!(l.kind, Some(QueryKind::Locals));
        assert_eq!(i.kind, Some(QueryKind::Injections));
    }

    #[test]
    fn should_parse_queries_array_in_language_config() {
        // LanguageSettings should have queries: Option<Vec<QueryItem>>
        let config_toml = r#"
            parser ="/path/to/parser.so"
            [[queries]]
            path = "/path/to/highlights.scm"

            [[queries]]
            path = "/path/to/locals.scm"
            kind = "locals"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();
        assert!(config.queries.is_some());
        let queries = config.queries.unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].path, "/path/to/highlights.scm");
        assert!(queries[0].kind.is_none());
        assert_eq!(queries[1].path, "/path/to/locals.scm");
        assert_eq!(queries[1].kind, Some(QueryKind::Locals));
    }

    // PBI-151 Subtask 2: Type inference for query kinds
    #[test]
    fn should_infer_highlights_from_filename_pattern() {
        // Only exact match "highlights.scm" -> Some(Highlights)
        assert_eq!(
            infer_query_kind("highlights.scm"),
            Some(QueryKind::Highlights)
        );
        assert_eq!(
            infer_query_kind("/path/to/highlights.scm"),
            Some(QueryKind::Highlights)
        );
        assert_eq!(
            infer_query_kind("/usr/share/python/highlights.scm"),
            Some(QueryKind::Highlights)
        );
        // Prefixed variants should NOT match (only exact filename)
        assert_eq!(infer_query_kind("./queries/python-highlights.scm"), None);
        assert_eq!(infer_query_kind("rust-highlights.scm"), None);
    }

    #[test]
    fn should_infer_locals_from_filename_pattern() {
        // Only exact match "locals.scm" -> Some(Locals)
        assert_eq!(infer_query_kind("locals.scm"), Some(QueryKind::Locals));
        assert_eq!(
            infer_query_kind("/path/to/locals.scm"),
            Some(QueryKind::Locals)
        );
        // Prefixed variants should NOT match (only exact filename)
        assert_eq!(infer_query_kind("./queries/rust-locals.scm"), None);
        assert_eq!(infer_query_kind("/usr/share/python-locals.scm"), None);
        assert_eq!(infer_query_kind("javascript-locals.scm"), None);
    }

    #[test]
    fn should_infer_injections_from_filename_pattern() {
        // Only exact match "injections.scm" -> Some(Injections)
        assert_eq!(
            infer_query_kind("injections.scm"),
            Some(QueryKind::Injections)
        );
        assert_eq!(
            infer_query_kind("/path/to/injections.scm"),
            Some(QueryKind::Injections)
        );
        // Prefixed variants should NOT match (only exact filename)
        assert_eq!(infer_query_kind("./markdown-injections.scm"), None);
        assert_eq!(infer_query_kind("/usr/share/markdown-injections.scm"), None);
        assert_eq!(infer_query_kind("rust-injections.scm"), None);
    }

    #[test]
    fn should_return_none_for_unrecognized_patterns() {
        // Files without highlights/locals/injections in the name should return None
        // (callers skip these files silently)
        assert_eq!(infer_query_kind("custom.scm"), None);
        assert_eq!(infer_query_kind("python.scm"), None);
        assert_eq!(infer_query_kind("/path/to/queries.scm"), None);
        assert_eq!(infer_query_kind("./custom-queries.scm"), None);
        assert_eq!(infer_query_kind("/usr/share/rust.scm"), None);
    }

    #[test]
    fn should_not_match_files_with_prefixes_before_pattern() {
        // Files like "local-injections.scm" should NOT match because they have
        // additional text before the pattern. Only exact matches like "injections.scm"
        // or suffix matches like "rust-injections.scm" should match.
        assert_eq!(
            infer_query_kind("local-injections.scm"),
            None,
            "local-injections.scm should not match injections pattern"
        );
        assert_eq!(
            infer_query_kind("global-locals.scm"),
            None,
            "global-locals.scm should not match locals pattern"
        );
        assert_eq!(
            infer_query_kind("custom-highlights.scm"),
            None,
            "custom-highlights.scm should not match highlights pattern"
        );
        // Files with multiple dashes before the pattern should also not match
        assert_eq!(
            infer_query_kind("very-local-injections.scm"),
            None,
            "very-local-injections.scm should not match"
        );
    }

    #[test]
    fn should_parse_aggregation_config_from_json() {
        let json = r#"{"priorities": ["server_a", "server_b"]}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.priorities,
            vec!["server_a".to_string(), "server_b".to_string()]
        );
    }

    #[test]
    fn should_parse_aggregation_config_empty_priorities() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert!(config.priorities.is_empty());
    }

    #[test]
    fn should_parse_aggregation_config_from_toml() {
        let toml_str = r#"priorities = ["server_a"]"#;
        let config: AggregationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.priorities, vec!["server_a".to_string()]);
    }

    #[test]
    fn should_resolve_aggregation_priorities_for_specific_method() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([
                (
                    "textDocument/completion".to_string(),
                    AggregationConfig {
                        priorities: vec!["server_a".to_string()],
                        ..Default::default()
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    AggregationConfig {
                        priorities: vec!["server_b".to_string()],
                        ..Default::default()
                    },
                ),
            ])),
        };
        let agg =
            config.resolve_aggregation("textDocument/completion", AggregationStrategy::Preferred);
        assert_eq!(agg.priorities, &["server_a".to_string()]);
    }

    #[test]
    fn should_resolve_aggregation_priorities_falls_back_to_wildcard() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    priorities: vec!["server_b".to_string()],
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert_eq!(agg.priorities, &["server_b".to_string()]);
    }

    #[test]
    fn should_resolve_aggregation_priorities_returns_empty_when_no_aggregation() {
        let config = BridgeLanguageConfig::default();
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert!(agg.priorities.is_empty());
    }

    #[test]
    fn should_resolve_aggregation_strategy_for_specific_method() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/diagnostic".to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Preferred),
                    ..Default::default()
                },
            )])),
        };
        let agg = config
            .resolve_aggregation("textDocument/diagnostic", AggregationStrategy::Concatenated);
        assert_eq!(agg.strategy, AggregationStrategy::Preferred);
    }

    #[test]
    fn should_resolve_aggregation_strategy_falls_back_to_wildcard() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Concatenated),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn should_resolve_aggregation_strategy_returns_default_when_no_aggregation() {
        let config = BridgeLanguageConfig::default();
        let agg = config
            .resolve_aggregation("textDocument/diagnostic", AggregationStrategy::Concatenated);
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn should_resolve_aggregation_strategy_returns_default_when_strategy_is_none() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/diagnostic".to_string(),
                AggregationConfig {
                    priorities: vec!["server_a".to_string()],
                    strategy: None,
                    ..Default::default()
                },
            )])),
        };
        let agg = config
            .resolve_aggregation("textDocument/diagnostic", AggregationStrategy::Concatenated);
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_for_specific_method() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([
                (
                    "textDocument/completion".to_string(),
                    AggregationConfig {
                        max_fan_out: Some(2),
                        ..Default::default()
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    AggregationConfig {
                        max_fan_out: Some(5),
                        ..Default::default()
                    },
                ),
            ])),
        };
        // Method-specific should win over wildcard
        let agg =
            config.resolve_aggregation("textDocument/completion", AggregationStrategy::Preferred);
        assert_eq!(agg.max_fan_out, Some(2));
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_falls_back_to_wildcard() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    max_fan_out: Some(3),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert_eq!(agg.max_fan_out, Some(3));
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_none_when_no_aggregation() {
        let config = BridgeLanguageConfig::default();
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_method_entry_without_field_does_not_fallback_to_wildcard()
     {
        // When a method-specific AggregationConfig exists but has max_fan_out: None,
        // the wildcard's max_fan_out does NOT apply. This is atomic-per-entry behavior:
        // the method key selects the entire AggregationConfig, not individual fields.
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([
                (
                    "textDocument/completion".to_string(),
                    AggregationConfig {
                        priorities: vec!["server_a".to_string()],
                        ..Default::default() // max_fan_out: None
                    },
                ),
                (
                    WILDCARD_KEY.to_string(),
                    AggregationConfig {
                        max_fan_out: Some(5),
                        ..Default::default()
                    },
                ),
            ])),
        };
        let agg =
            config.resolve_aggregation("textDocument/completion", AggregationStrategy::Preferred);
        assert_eq!(
            agg.max_fan_out, None,
            "method-specific entry without maxFanOut should NOT fall back to wildcard"
        );
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_negative_as_none() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    max_fan_out: Some(-1),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn should_resolve_aggregation_max_fan_out_zero_as_some_zero() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                WILDCARD_KEY.to_string(),
                AggregationConfig {
                    max_fan_out: Some(0),
                    ..Default::default()
                },
            )])),
        };
        let agg = config.resolve_aggregation("textDocument/hover", AggregationStrategy::Preferred);
        assert_eq!(agg.max_fan_out, Some(0));
    }

    #[test]
    fn should_resolve_aggregation_all_fields_together() {
        let config = BridgeLanguageConfig {
            enabled: Some(true),
            aggregation: Some(HashMap::from([(
                "textDocument/completion".to_string(),
                AggregationConfig {
                    strategy: Some(AggregationStrategy::Concatenated),
                    priorities: vec!["server_a".to_string(), "server_b".to_string()],
                    max_fan_out: Some(3),
                },
            )])),
        };
        let agg =
            config.resolve_aggregation("textDocument/completion", AggregationStrategy::Preferred);
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
        assert_eq!(agg.priorities, vec!["server_a", "server_b"]);
        assert_eq!(agg.max_fan_out, Some(3));
    }

    #[test]
    fn should_resolve_aggregation_with_defaults_uses_given_strategy() {
        let agg = ResolvedAggregationConfig::with_defaults(AggregationStrategy::Concatenated);
        assert_eq!(agg.strategy, AggregationStrategy::Concatenated);
        assert!(agg.priorities.is_empty());
        assert_eq!(agg.max_fan_out, None);

        let agg = ResolvedAggregationConfig::with_defaults(AggregationStrategy::Preferred);
        assert_eq!(agg.strategy, AggregationStrategy::Preferred);
        assert!(agg.priorities.is_empty());
        assert_eq!(agg.max_fan_out, None);
    }

    #[test]
    fn should_parse_bridge_language_config_with_aggregation() {
        let json = r#"{
            "enabled": true,
            "aggregation": {
                "textDocument/completion": { "priorities": ["server_a"] },
                "_": { "priorities": ["server_b"] }
            }
        }"#;
        let config: BridgeLanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.enabled, Some(true));
        let agg = config.aggregation.as_ref().unwrap();
        assert_eq!(agg.len(), 2);
        assert_eq!(
            agg["textDocument/completion"].priorities,
            vec!["server_a".to_string()]
        );
        assert_eq!(agg["_"].priorities, vec!["server_b".to_string()]);
    }

    #[test]
    fn should_parse_bridge_language_config_enabled_defaults_to_none() {
        // When enabled is omitted, it should be None (for wildcard inheritance)
        let json = r#"{}"#;
        let config: BridgeLanguageConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.enabled, None);
        assert!(config.aggregation.is_none());
    }

    #[test]
    fn should_parse_aggregation_strategy_preferred() {
        let json = r#"{ "strategy": "preferred" }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::Preferred));
    }

    #[test]
    fn should_parse_aggregation_strategy_concatenated() {
        let json = r#"{ "strategy": "concatenated" }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::Concatenated));
    }

    #[test]
    fn should_parse_aggregation_config_without_strategy() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, None);
    }

    #[test]
    fn should_parse_max_fan_out_with_value() {
        let json = r#"{ "maxFanOut": 2 }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_fan_out, Some(2));
    }

    #[test]
    fn should_parse_max_fan_out_null_as_none() {
        let json = r#"{ "maxFanOut": null }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_fan_out, None);
    }

    #[test]
    fn should_parse_max_fan_out_absent_as_none() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.max_fan_out, None);
    }

    #[test]
    fn should_parse_aggregation_strategy_from_toml() {
        let toml_str = r#"strategy = "preferred""#;
        let config: AggregationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::Preferred));
    }

    #[test]
    fn should_parse_language_config_with_aliases() {
        // aliases allows mapping multiple languageId values to one parser
        // Example: markdown parser handles rmd, qmd, mdx files
        let config_toml = r#"
            parser = "/path/to/markdown.so"
            aliases = ["rmd", "qmd", "mdx"]

            [[queries]]
            path = "/path/to/highlights.scm"
            kind = "highlights"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_some(), "aliases field should be present");
        let aliases = config.aliases.unwrap();
        assert_eq!(aliases.len(), 3);
        assert_eq!(aliases[0], "rmd");
        assert_eq!(aliases[1], "qmd");
        assert_eq!(aliases[2], "mdx");
    }

    #[test]
    fn should_parse_language_config_without_aliases() {
        // When aliases is omitted, it should be None
        let config_toml = r#"
            parser = "/path/to/rust.so"

            [[queries]]
            path = "/path/to/highlights.scm"
            kind = "highlights"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_none(), "omitted aliases should be None");
    }

    #[test]
    fn should_parse_language_config_with_empty_aliases() {
        // Empty aliases array should be Some([])
        let config_toml = r#"
            parser ="/path/to/parser.so"
            aliases = []
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_some(), "empty aliases should be Some");
        assert!(
            config.aliases.as_ref().unwrap().is_empty(),
            "should be empty vec"
        );
    }

    #[test]
    fn should_parse_language_config_with_base() {
        let config_toml = r#"
            base = "markdown"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert_eq!(config.base, Some("markdown".to_string()));
        assert!(config.parser.is_none());
    }

    #[test]
    fn should_parse_language_config_without_base() {
        let config_toml = r#"
            parser = "/path/to/rust.so"
        "#;

        let config: LanguageSettings = toml::from_str(config_toml).unwrap();

        assert!(config.base.is_none());
    }
}
