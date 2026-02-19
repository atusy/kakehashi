use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

pub type CaptureMapping = HashMap<String, String>;

/// Workspace type for bridge language server connections.
///
/// Determines the project structure created in the temp directory:
/// - Cargo: Creates Cargo.toml and src/main.rs (for rust-analyzer)
/// - Generic: Creates only a virtual.<ext> file (for language servers that don't need project structure)
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceType {
    /// Cargo workspace with Cargo.toml and src/main.rs
    Cargo,
    /// Generic workspace with just a virtual file
    Generic,
}

/// Aggregation strategy for combining results from multiple bridge servers.
///
/// - `Preferred`: Use the first non-empty response (priority-ordered).
///   This is the default for most LSP methods.
/// - `All`: Collect and merge responses from all servers.
///   This is the default for `textDocument/diagnostic`.
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AggregationStrategy {
    Preferred,
    All,
}

/// Per-method aggregation configuration.
///
/// Controls how results from multiple bridge servers are aggregated for a
/// specific LSP method. The `priorities` list determines server preference
/// order — the first server in the list that returns a non-empty result wins.
/// Empty priorities degrades to first-win (arrival-order) behavior.
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq, Default)]
pub(crate) struct AggregationConfig {
    /// Server names in priority order (highest first). Empty = pure first-win behavior.
    #[serde(default)]
    pub(crate) priorities: Vec<String>,
    /// Aggregation strategy override. `None` = use handler default.
    #[serde(default)]
    pub(crate) strategy: Option<AggregationStrategy>,
}

/// Configuration for a single bridged language within a host filetype.
///
/// Used in the bridge filter map to control whether a specific injection language
/// should be bridged and how results are aggregated.
/// Example: `{ python = { enabled = true, aggregation = { "_" = { priorities = ["pyright"] } } } }`.
///
/// All fields are `Option` to support wildcard inheritance (ADR-0011):
/// `None` means "inherit from wildcard `_` key".
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq, Default)]
pub struct BridgeLanguageConfig {
    /// Whether bridging is enabled for this language.
    /// `None` = inherit from wildcard (defaults to `true`).
    pub(crate) enabled: Option<bool>,
    /// Per-method aggregation config. Key = LSP method name or "_" for default.
    pub(crate) aggregation: Option<HashMap<String, AggregationConfig>>,
}

impl BridgeLanguageConfig {
    /// Resolve priorities for a specific LSP method.
    ///
    /// Falls back to wildcard `"_"` key, then empty slice.
    pub(crate) fn resolve_priorities(&self, method: &str) -> Vec<String> {
        let Some(map) = self.aggregation.as_ref() else {
            return vec![];
        };
        map.get(method)
            .or_else(|| map.get(crate::config::WILDCARD_KEY))
            .map(|c| c.priorities.clone())
            .unwrap_or_default()
    }
}

/// Configuration for a bridge language server.
///
/// This is used to configure external language servers (like rust-analyzer, pyright)
/// that kakehashi can redirect requests to for injection regions.
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct BridgeServerConfig {
    /// Command array: first element is the program, rest are arguments
    /// e.g., ["rust-analyzer"] or ["pyright-langserver", "--stdio"]
    pub cmd: Vec<String>,
    /// Languages this server handles (e.g., ["rust"], ["python"])
    pub languages: Vec<String>,
    /// Optional initialization options to pass to the server during initialize
    #[serde(rename = "initializationOptions")]
    pub initialization_options: Option<Value>,
    /// Workspace type for this server (defaults to None, meaning Generic)
    #[serde(rename = "workspaceType")]
    pub workspace_type: Option<WorkspaceType>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, Default, PartialEq, Eq)]
pub struct QueryTypeMappings {
    #[serde(default)]
    pub highlights: CaptureMapping,
    #[serde(default)]
    pub locals: CaptureMapping,
    #[serde(default)]
    pub folds: CaptureMapping,
}

pub type CaptureMappings = HashMap<String, QueryTypeMappings>;

/// Query type for tree-sitter query files.
///
/// Used in the unified `queries` field to specify what kind of query a file contains.
/// When not specified, the kind is inferred from the filename pattern.
#[derive(Debug, Clone, Copy, Deserialize, serde::Serialize, PartialEq, Eq)]
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
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
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
/// - Otherwise -> `None` (unknown patterns are skipped by callers)
///
/// Examples:
/// - `injections.scm` -> matches
/// - `rust-injections.scm` -> does NOT match (only exact filename matches)
/// - `local-injections.scm` -> does NOT match (only exact filename matches)
pub fn infer_query_kind(path: &str) -> Option<QueryKind> {
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

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct LanguageConfig {
    pub parser: Option<String>,
    /// Unified query file configuration.
    /// Each entry has a path and optional kind (inferred from filename if omitted).
    pub queries: Option<Vec<QueryItem>>,
    /// Languages to bridge for this host filetype (map format).
    /// - None (omitted): Bridge ALL configured languages (default behavior)
    /// - Some({}): Bridge NOTHING (disable bridging for this host)
    /// - Some({ python: { enabled: true } }): Bridge only enabled languages
    pub bridge: Option<HashMap<String, BridgeLanguageConfig>>,
    /// Alternative languageId values that map to this language.
    /// Example: `[languages.markdown]` with `aliases = ["rmd", "qmd"]`
    /// allows editors sending languageId "rmd" or "qmd" to use the markdown parser.
    pub aliases: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct TreeSitterSettings {
    // Editor-agnostic name exposed to JSON as `searchPaths`.
    #[serde(rename = "searchPaths")]
    pub search_paths: Option<Vec<String>>,
    #[serde(default)]
    pub languages: HashMap<String, LanguageConfig>,
    #[serde(rename = "captureMappings", default)]
    pub capture_mappings: CaptureMappings,
    #[serde(rename = "autoInstall")]
    pub auto_install: Option<bool>,
    /// Language servers for bridging LSP requests to injection regions.
    /// Map of server name to server configuration.
    #[serde(rename = "languageServers")]
    pub language_servers: Option<HashMap<String, BridgeServerConfig>>,
}

// Domain types - internal representations used throughout the application

/// Per-language Tree-sitter language configuration surfaced to the domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageSettings {
    /// Path to the parser library (.so/.dylib/.dll)
    pub parser: Option<String>,
    /// Unified query file configuration
    /// - None: Not specified (inherit from wildcard/defaults)
    /// - Some([]): Explicitly empty (override wildcard with no queries)
    /// - Some([...]): Specified queries
    pub queries: Option<Vec<QueryItem>>,
    /// Languages to bridge for this host filetype (map format).
    /// - None (omitted): Bridge ALL configured languages (default behavior)
    /// - Some({}): Bridge NOTHING (disable bridging for this host)
    /// - Some({ python: { enabled: true } }): Bridge only enabled languages
    pub bridge: Option<HashMap<String, BridgeLanguageConfig>>,
    /// Alternative languageId values that should use this parser.
    /// E.g., `aliases = ["rmd", "qmd"]` for markdown allows editors sending
    /// "rmd" or "qmd" as languageId to use the markdown parser.
    pub aliases: Option<Vec<String>>,
}

impl LanguageSettings {
    pub fn new(parser: Option<String>, queries: Option<Vec<QueryItem>>) -> Self {
        Self {
            parser,
            queries,
            bridge: None,
            aliases: None,
        }
    }

    /// Create LanguageSettings with bridge filter configuration.
    pub fn with_bridge(
        parser: Option<String>,
        queries: Option<Vec<QueryItem>>,
        bridge: Option<HashMap<String, BridgeLanguageConfig>>,
    ) -> Self {
        Self {
            parser,
            queries,
            bridge,
            aliases: None,
        }
    }

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
                let resolved =
                    crate::config::resolve_bridge_language_with_wildcard(map, injection_language);
                resolved.map(|c| c.enabled.unwrap_or(true)).unwrap_or(false)
            }
        }
    }
}

/// Workspace-wide Tree-sitter configuration as required by the domain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkspaceSettings {
    pub search_paths: Vec<String>,
    pub languages: HashMap<String, LanguageSettings>,
    pub capture_mappings: CaptureMappings,
    pub auto_install: bool,
    pub language_servers: Option<HashMap<String, BridgeServerConfig>>,
}

impl WorkspaceSettings {
    pub fn new(
        search_paths: Vec<String>,
        languages: HashMap<String, LanguageSettings>,
        capture_mappings: CaptureMappings,
    ) -> Self {
        Self {
            search_paths,
            languages,
            capture_mappings,
            auto_install: true, // Default to true for zero-config experience
            language_servers: None,
        }
    }

    pub fn with_auto_install(
        search_paths: Vec<String>,
        languages: HashMap<String, LanguageSettings>,
        capture_mappings: CaptureMappings,
        auto_install: bool,
    ) -> Self {
        Self {
            search_paths,
            languages,
            capture_mappings,
            auto_install,
            language_servers: None,
        }
    }

    pub fn with_language_servers(
        search_paths: Vec<String>,
        languages: HashMap<String, LanguageSettings>,
        capture_mappings: CaptureMappings,
        auto_install: bool,
        language_servers: Option<HashMap<String, BridgeServerConfig>>,
    ) -> Self {
        Self {
            search_paths,
            languages,
            capture_mappings,
            auto_install,
            language_servers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WILDCARD_KEY;
    use rstest::rstest;

    #[test]
    fn should_distinguish_between_unspecified_and_empty_queries() {
        // This test demonstrates the need for Option<Vec<QueryItem>>
        // to distinguish between "not specified" and "explicitly empty"
        // which is critical for merging logic in resolve_language_settings_with_wildcard

        // Case 1: queries not specified (should be None)
        // User didn't specify queries - should inherit from wildcard/defaults
        let unspecified = LanguageSettings::new(None, None);
        assert!(
            unspecified.queries.is_none(),
            "Unspecified queries should be None"
        );

        // Case 2: queries explicitly empty (should be Some([]))
        // User explicitly set queries to empty - should override wildcard with empty list
        let explicitly_empty = LanguageSettings::new(None, Some(vec![]));
        assert!(
            explicitly_empty.queries.is_some(),
            "Explicitly empty should be Some"
        );
        assert!(
            explicitly_empty.queries.as_ref().unwrap().is_empty(),
            "Should be empty vec"
        );

        // Case 3: queries with items
        let with_items = LanguageSettings::new(
            None,
            Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
        );
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

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

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

    #[test]
    fn should_parse_configuration_with_locals_query() {
        // Configuration using unified queries field with locals
        let config_json = r#"{
            "languages": {
                "rust": {
                    "queries": [
                        {"path": "/path/to/highlights.scm", "kind": "highlights"},
                        {"path": "/path/to/locals.scm", "kind": "locals"}
                    ]
                }
            }
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.languages.contains_key("rust"));
        let rust_config = &settings.languages["rust"];

        // Verify queries configuration is parsed
        let queries = rust_config.queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[1].path, "/path/to/locals.scm");
        assert_eq!(queries[1].kind, Some(QueryKind::Locals));
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

        let result = serde_json::from_str::<TreeSitterSettings>(invalid_json);
        assert!(result.is_err());
    }

    #[test]
    fn should_handle_empty_configurations() {
        let empty_json = r#"{
            "languages": {}
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(empty_json).unwrap();
        assert!(settings.languages.is_empty());
    }

    #[test]
    fn should_handle_completely_empty_json_object() {
        // This is crucial for zero-config: InitializationOptions = {} should work
        let completely_empty = r#"{}"#;

        let settings: TreeSitterSettings = serde_json::from_str(completely_empty).unwrap();
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

        let settings: TreeSitterSettings = serde_json::from_str(json_without_languages).unwrap();
        assert!(settings.languages.is_empty());
        assert_eq!(settings.search_paths, Some(vec!["/some/path".to_string()]));
    }

    #[test]
    fn should_parse_toml_without_languages_field() {
        let toml_without_languages = r#"
            [captureMappings._.highlights]
        "#;

        let settings: TreeSitterSettings = toml::from_str(toml_without_languages).unwrap();
        assert!(settings.languages.is_empty());
    }

    #[test]
    fn should_parse_searchpaths_configuration_basic() {
        let config_json = r#"{
            "searchPaths": [
                "/usr/local/lib/tree-sitter",
                "/opt/tree-sitter/parsers"
            ],
            "languages": {
                "rust": {
                    "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
                }
            }
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.search_paths.is_some());
        assert_eq!(
            settings.search_paths.unwrap(),
            vec!["/usr/local/lib/tree-sitter", "/opt/tree-sitter/parsers"]
        );
        assert!(settings.languages.contains_key("rust"));
        assert_eq!(settings.languages["rust"].parser, None);
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

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

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
    fn should_parse_searchpaths_configuration() {
        let config_json = r#"{
            "searchPaths": [
                "/data/tree-sitter",
                "/assets/ts"
            ],
            "languages": {
                "lua": {
                    "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}]
                }
            }
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert_eq!(
            settings.search_paths.unwrap(),
            vec!["/data/tree-sitter", "/assets/ts"]
        );
        assert!(settings.languages.contains_key("lua"));
    }

    #[test]
    fn should_handle_malformed_json_gracefully() {
        let malformed_configs = vec![
            r#"{"languages": {"rust": {"parser": "/path"}"#, // Missing closing braces
            r#"{"languages": {"rust": {"parser": "/path", "queries": [}}"#, // Invalid array
        ];

        for config in malformed_configs {
            let result = serde_json::from_str::<TreeSitterSettings>(config);
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

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        // Check capture mappings are parsed correctly
        assert!(settings.capture_mappings.contains_key(WILDCARD_KEY));
        assert!(settings.capture_mappings.contains_key("rust"));

        let wildcard_mappings = &settings.capture_mappings[WILDCARD_KEY].highlights;
        assert_eq!(
            wildcard_mappings.get("variable.builtin"),
            Some(&"variable.defaultLibrary".to_string())
        );
        assert_eq!(
            wildcard_mappings.get("function.builtin"),
            Some(&"function.defaultLibrary".to_string())
        );

        let rust_mappings = &settings.capture_mappings["rust"].highlights;
        assert_eq!(
            rust_mappings.get("type.builtin"),
            Some(&"type.defaultLibrary".to_string())
        );

        let rust_locals = &settings.capture_mappings["rust"].locals;
        assert_eq!(
            rust_locals.get("definition.var"),
            Some(&"definition.variable".to_string())
        );
    }

    #[test]
    fn should_parse_queries_as_vec_query_item() {
        // Unified queries field with multiple entries
        let config_json = r#"{
            "languages": {
                "lua": {
                    "parser": "/path/to/lua.so",
                    "queries": [
                        {"path": "/path/to/highlights.scm", "kind": "highlights"},
                        {"path": "/path/to/custom.scm"}
                    ]
                }
            }
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.languages.contains_key("lua"));
        let lua_config = &settings.languages["lua"];
        assert_eq!(lua_config.parser, Some("/path/to/lua.so".to_string()));

        let queries = lua_config.queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[0].path, "/path/to/highlights.scm");
        assert_eq!(queries[0].kind, Some(QueryKind::Highlights));
        assert_eq!(queries[1].path, "/path/to/custom.scm");
        assert_eq!(queries[1].kind, None);
    }

    #[test]
    fn should_parse_auto_install_setting() {
        let config_json = r#"{
            "autoInstall": true,
            "languages": {}
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();
        assert_eq!(settings.auto_install, Some(true));

        // Test with false
        let config_false = r#"{
            "autoInstall": false,
            "languages": {}
        }"#;
        let settings_false: TreeSitterSettings = serde_json::from_str(config_false).unwrap();
        assert_eq!(settings_false.auto_install, Some(false));

        // Test missing (should be None)
        let config_missing = r#"{
            "languages": {}
        }"#;
        let settings_missing: TreeSitterSettings = serde_json::from_str(config_missing).unwrap();
        assert_eq!(settings_missing.auto_install, None);
    }

    #[test]
    fn should_handle_complex_configurations_efficiently() {
        let mut config = TreeSitterSettings {
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
                LanguageConfig {
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
        let deserialized: TreeSitterSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.languages.len(), config.languages.len());
    }

    #[test]
    fn should_not_have_legacy_fields_in_language_config() {
        // Legacy highlights/locals/injections fields have been removed from LanguageConfig
        // All query configuration now uses the unified queries field
        let config = LanguageConfig {
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
            "LanguageConfig should not have highlights field, but found in JSON: {}",
            json
        );
        assert!(
            !json.contains("\"locals\":"),
            "LanguageConfig should not have locals field, but found in JSON: {}",
            json
        );
        assert!(
            !json.contains("\"injections\":"),
            "LanguageConfig should not have injections field, but found in JSON: {}",
            json
        );
    }

    #[test]
    fn should_create_language_settings_with_parser_and_queries() {
        // PBI-156: LanguageSettings uses parser (not library) and unified queries
        let settings = LanguageSettings::new(
            Some("/path/to/parser.so".to_string()),
            Some(vec![QueryItem {
                path: "/path/to/highlights.scm".to_string(),
                kind: Some(QueryKind::Highlights),
            }]),
        );

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
    fn should_parse_configuration_with_injections_query() {
        // Test that injection queries can be specified in the unified queries field
        let config_json = r#"{
            "languages": {
                "markdown": {
                    "queries": [
                        {"path": "/path/to/highlights.scm", "kind": "highlights"},
                        {"path": "/path/to/injections.scm", "kind": "injections"}
                    ]
                }
            }
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.languages.contains_key("markdown"));
        let md_config = &settings.languages["markdown"];

        // Verify queries configuration is parsed
        let queries = md_config.queries.as_ref().unwrap();
        assert_eq!(queries.len(), 2);
        assert_eq!(queries[1].path, "/path/to/injections.scm");
        assert_eq!(queries[1].kind, Some(QueryKind::Injections));
    }

    #[rstest]
    #[case::cargo(
        r#"{"cmd": ["rust-analyzer"], "languages": ["rust"], "workspaceType": "cargo"}"#,
        Some(WorkspaceType::Cargo)
    )]
    #[case::generic(
        r#"{"cmd": ["pyright-langserver", "--stdio"], "languages": ["python"], "workspaceType": "generic"}"#,
        Some(WorkspaceType::Generic),
    )]
    #[case::defaults_to_none(r#"{"cmd": ["rust-analyzer"], "languages": ["rust"]}"#, None)]
    fn should_parse_bridge_server_config_workspace_type(
        #[case] config_json: &str,
        #[case] expected: Option<WorkspaceType>,
    ) {
        let config: BridgeServerConfig = serde_json::from_str(config_json).unwrap();
        assert_eq!(config.workspace_type, expected);
    }

    #[test]
    fn should_parse_language_config_with_bridge_map_enabled() {
        // PBI-120: LanguageConfig should parse bridge field as HashMap<String, BridgeLanguageConfig>
        // Example: bridge = { python = { enabled = true }, r = { enabled = true } }
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {
                "python": { "enabled": true },
                "r": { "enabled": true }
            }
        }"#;

        let config: LanguageConfig = serde_json::from_str(config_json).unwrap();

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

        let config: LanguageConfig = serde_json::from_str(config_json).unwrap();

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

        let config: LanguageConfig = serde_json::from_str(config_json).unwrap();

        assert!(
            config.bridge.is_none(),
            "omitted bridge field should be None"
        );
    }

    #[test]
    fn test_bridge_filter_null_bridges_all_languages() {
        // PBI-108 AC3: bridge omitted or null bridges all configured languages
        // When bridge is None (default), all languages should be bridgeable
        let settings = LanguageSettings::new(None, None);

        // Default (None) should bridge all languages
        assert!(
            settings.is_language_bridgeable("python"),
            "None bridge should allow python"
        );
        assert!(
            settings.is_language_bridgeable("r"),
            "None bridge should allow r"
        );
        assert!(
            settings.is_language_bridgeable("rust"),
            "None bridge should allow rust"
        );
        assert!(
            settings.is_language_bridgeable("any_language"),
            "None bridge should allow any language"
        );
    }

    #[test]
    fn test_bridge_filter_empty_disables_bridging() {
        // PBI-120: Empty bridge map disables all bridging for that host filetype
        let settings = LanguageSettings::with_bridge(
            None,
            None,
            Some(HashMap::new()), // Empty map disables all bridging
        );

        // Empty map should disable all bridging
        assert!(
            !settings.is_language_bridgeable("python"),
            "Empty bridge should not allow python"
        );
        assert!(
            !settings.is_language_bridgeable("r"),
            "Empty bridge should not allow r"
        );
        assert!(
            !settings.is_language_bridgeable("rust"),
            "Empty bridge should not allow rust"
        );
    }

    #[test]
    fn test_bridge_filter_allows_enabled_languages() {
        // PBI-120: Only languages with enabled: true should be bridgeable
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        bridge.insert(
            "r".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );

        let settings = LanguageSettings::with_bridge(None, None, Some(bridge));

        // Enabled languages should be allowed
        assert!(
            settings.is_language_bridgeable("python"),
            "python should be in bridge filter"
        );
        assert!(
            settings.is_language_bridgeable("r"),
            "r should be in bridge filter"
        );

        // Languages not in map should NOT be allowed
        assert!(
            !settings.is_language_bridgeable("rust"),
            "rust should not be in bridge filter"
        );
        assert!(
            !settings.is_language_bridgeable("javascript"),
            "javascript should not be in bridge filter"
        );
    }

    #[test]
    fn test_bridge_filter_disabled_language() {
        // PBI-120: Languages with enabled: false should not be bridgeable
        let mut bridge = HashMap::new();
        bridge.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        bridge.insert(
            "r".to_string(),
            BridgeLanguageConfig {
                enabled: Some(false),
                ..Default::default()
            },
        );

        let settings = LanguageSettings::with_bridge(None, None, Some(bridge));

        // python with enabled: true should be allowed
        assert!(
            settings.is_language_bridgeable("python"),
            "python should be bridgeable"
        );
        // r with enabled: false should NOT be allowed
        assert!(
            !settings.is_language_bridgeable("r"),
            "r with enabled: false should not be bridgeable"
        );
    }

    #[test]
    fn should_parse_language_servers_at_root() {
        // PBI-119: languageServers field should be at root level of InitializationOptions
        // This replaces the nested bridge.servers structure with a flatter schema
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib"],
            "languageServers": {
                "rust-analyzer": {
                    "cmd": ["rust-analyzer"],
                    "languages": ["rust"],
                    "workspaceType": "cargo"
                },
                "pyright": {
                    "cmd": ["pyright-langserver", "--stdio"],
                    "languages": ["python"]
                }
            }
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.language_servers.is_some());
        let servers = settings.language_servers.as_ref().unwrap();
        assert_eq!(servers.len(), 2);

        // Check rust-analyzer config
        assert!(servers.contains_key("rust-analyzer"));
        let ra = &servers["rust-analyzer"];
        assert_eq!(ra.cmd, vec!["rust-analyzer".to_string()]);
        assert_eq!(ra.languages, vec!["rust".to_string()]);
        assert_eq!(ra.workspace_type, Some(WorkspaceType::Cargo));

        // Check pyright config
        assert!(servers.contains_key("pyright"));
        let py = &servers["pyright"];
        assert_eq!(
            py.cmd,
            vec!["pyright-langserver".to_string(), "--stdio".to_string()]
        );
        assert_eq!(py.languages, vec!["python".to_string()]);
    }

    #[test]
    fn should_parse_language_servers_empty() {
        // PBI-119: Empty languageServers should be valid
        let config_json = r#"{
            "languageServers": {}
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

        assert!(settings.language_servers.is_some());
        assert!(settings.language_servers.as_ref().unwrap().is_empty());
    }

    #[test]
    fn should_parse_without_language_servers() {
        // PBI-119: Missing languageServers should be None (backward compatibility)
        let config_json = r#"{
            "searchPaths": ["/usr/local/lib"]
        }"#;

        let settings: TreeSitterSettings = serde_json::from_str(config_json).unwrap();

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
        // PBI-120: LanguageConfig.bridge should be HashMap<String, BridgeLanguageConfig>
        // Example: bridge = { python = { enabled = true }, r = { enabled = false } }
        let config_json = r#"{
            "parser": "/path/to/parser.so",
            "queries": [{"path": "/path/to/highlights.scm", "kind": "highlights"}],
            "bridge": {
                "python": { "enabled": true },
                "r": { "enabled": false }
            }
        }"#;

        let config: LanguageConfig = serde_json::from_str(config_json).unwrap();

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
        // LanguageConfig should have queries: Option<Vec<QueryItem>>
        let config_toml = r#"
            parser ="/path/to/parser.so"
            [[queries]]
            path = "/path/to/highlights.scm"

            [[queries]]
            path = "/path/to/locals.scm"
            kind = "locals"
        "#;

        let config: LanguageConfig = toml::from_str(config_toml).unwrap();
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
    fn should_resolve_priorities_for_specific_method() {
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
        assert_eq!(
            config.resolve_priorities("textDocument/completion"),
            &["server_a".to_string()]
        );
    }

    #[test]
    fn should_resolve_priorities_falls_back_to_wildcard() {
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
        assert_eq!(
            config.resolve_priorities("textDocument/hover"),
            &["server_b".to_string()]
        );
    }

    #[test]
    fn should_resolve_priorities_returns_empty_when_no_aggregation() {
        let config = BridgeLanguageConfig::default();
        assert!(config.resolve_priorities("textDocument/hover").is_empty());
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
    fn should_parse_aggregation_strategy_all() {
        let json = r#"{ "strategy": "all" }"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, Some(AggregationStrategy::All));
    }

    #[test]
    fn should_parse_aggregation_config_without_strategy() {
        let json = r#"{}"#;
        let config: AggregationConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.strategy, None);
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

        let config: LanguageConfig = toml::from_str(config_toml).unwrap();

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

        let config: LanguageConfig = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_none(), "omitted aliases should be None");
    }

    #[test]
    fn should_parse_language_config_with_empty_aliases() {
        // Empty aliases array should be Some([])
        let config_toml = r#"
            parser ="/path/to/parser.so"
            aliases = []
        "#;

        let config: LanguageConfig = toml::from_str(config_toml).unwrap();

        assert!(config.aliases.is_some(), "empty aliases should be Some");
        assert!(
            config.aliases.as_ref().unwrap().is_empty(),
            "should be empty vec"
        );
    }
}
