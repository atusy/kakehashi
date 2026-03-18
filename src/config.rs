pub mod defaults;
pub(crate) mod expand;
pub mod settings;

#[cfg(test)]
pub(crate) use expand::make_env;
pub(crate) mod user;

pub use expand::{set_config_file_override, set_data_dir_override};
pub(crate) use settings::{CaptureMappings, QueryTypeMappings};
pub use settings::{LanguageSettings, RawWorkspaceSettings, WorkspaceSettings, json_schema};
use std::collections::HashMap;
pub(crate) use user::load_user_config;

/// Wildcard key for default configurations in HashMap-based settings.
/// Used in capture_mappings, languages, and language_servers for fallback values.
pub(crate) const WILDCARD_KEY: &str = "_";

/// Resolve a key from a map with wildcard fallback and merging.
///
/// Implements ADR-0011 wildcard config inheritance for HashMap-based settings:
/// - If both wildcard ("_") and specific key exist: merge them via `merge`
/// - If only wildcard exists: return wildcard (cloned)
/// - If only specific key exists: return specific key (cloned)
/// - If neither exists: return None
pub(crate) fn resolve_with_wildcard<V: Clone>(
    map: &HashMap<String, V>,
    key: &str,
    merge: impl Fn(&V, &V) -> V,
) -> Option<V> {
    let wildcard = map.get(WILDCARD_KEY);
    let specific = map.get(key);
    match (wildcard, specific) {
        (Some(w), Some(s)) => Some(merge(w, s)),
        (Some(w), None) => Some(w.clone()),
        (None, Some(s)) => Some(s.clone()),
        (None, None) => None,
    }
}

/// Field-level merge of two BridgeLanguageConfig values.
/// Overlay fields win when present; base provides defaults.
pub(crate) fn merge_bridge_language_configs(
    base: &settings::BridgeLanguageConfig,
    overlay: &settings::BridgeLanguageConfig,
) -> settings::BridgeLanguageConfig {
    settings::BridgeLanguageConfig {
        enabled: overlay.enabled.or(base.enabled),
        aggregation: match (&base.aggregation, &overlay.aggregation) {
            (Some(base_agg), Some(overlay_agg)) => {
                let mut merged = base_agg.clone();
                merged.extend(overlay_agg.clone());
                Some(merged)
            }
            (base_agg, overlay_agg) => overlay_agg.clone().or_else(|| base_agg.clone()),
        },
    }
}

/// Field-level merge of two BridgeServerConfig values.
/// Vec fields: use overlay if non-empty, else base.
/// JSON Option fields: deep merge (ADR-0010).
/// Option fields: overlay wins when present.
pub(crate) fn merge_bridge_server_configs(
    base: &settings::BridgeServerConfig,
    overlay: &settings::BridgeServerConfig,
) -> settings::BridgeServerConfig {
    settings::BridgeServerConfig {
        cmd: if overlay.cmd.is_empty() {
            base.cmd.clone()
        } else {
            overlay.cmd.clone()
        },
        languages: if overlay.languages.is_empty() {
            base.languages.clone()
        } else {
            overlay.languages.clone()
        },
        initialization_options: match (
            &base.initialization_options,
            &overlay.initialization_options,
        ) {
            (Some(b), Some(o)) => Some(deep_merge_json(b, o)),
            _ => overlay
                .initialization_options
                .clone()
                .or(base.initialization_options.clone()),
        },
        workspace_type: overlay.workspace_type.or(base.workspace_type),
    }
}

/// Field-level merge of two LanguageSettings values.
/// Option fields: overlay wins when present; base provides defaults.
/// Bridge HashMaps: deep merged via merge_bridge_maps.
/// Aliases: not inherited from wildcard (specific to each language).
pub(crate) fn merge_language_settings(
    base: &LanguageSettings,
    overlay: &LanguageSettings,
) -> LanguageSettings {
    LanguageSettings {
        parser: overlay.parser.clone().or_else(|| base.parser.clone()),
        queries: overlay.queries.clone().or_else(|| base.queries.clone()),
        bridge: merge_bridge_maps(&base.bridge, &overlay.bridge),
        aliases: overlay.aliases.clone(),
    }
}

/// Deep merge two optional bridge HashMaps.
///
/// When both base and overlay exist:
/// - If overlay is empty (`Some({})`), it completely replaces base (empty-means-clear).
///   This preserves the "empty map = disable all bridging" contract.
/// - Otherwise, per-key entries are merged at the field level using
///   [`merge_bridge_language_configs`].
fn merge_bridge_maps(
    base: &Option<HashMap<String, settings::BridgeLanguageConfig>>,
    overlay: &Option<HashMap<String, settings::BridgeLanguageConfig>>,
) -> Option<HashMap<String, settings::BridgeLanguageConfig>> {
    match (base, overlay) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(_), Some(o)) if o.is_empty() => Some(HashMap::new()), // empty-means-clear
        (Some(b), Some(o)) => {
            let mut merged = b.clone();
            for (key, overlay_config) in o {
                merged
                    .entry(key.clone())
                    .and_modify(|base_config| {
                        *base_config = merge_bridge_language_configs(base_config, overlay_config);
                    })
                    .or_insert_with(|| overlay_config.clone());
            }
            Some(merged)
        }
    }
}

/// Deep merge two JSON values (ADR-0010).
///
/// For objects: recursively merge keys, with `overlay` values taking precedence.
/// For non-objects: `overlay` completely replaces `base`.
///
/// This implements the deep merge semantics required for initialization_options:
/// - If both are objects, merge their keys recursively
/// - If either is not an object, overlay wins (including null values)
fn deep_merge_json(base: &serde_json::Value, overlay: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (key, overlay_value) in overlay_map {
                merged
                    .entry(key.clone())
                    .and_modify(|base_value| {
                        *base_value = deep_merge_json(base_value, overlay_value);
                    })
                    .or_insert_with(|| overlay_value.clone());
            }
            Value::Object(merged)
        }
        // For non-objects, overlay completely replaces base
        _ => overlay.clone(),
    }
}

/// Returns the default search paths for parsers and queries.
/// Uses the platform-specific data directory (via `dirs` crate):
/// - Linux: ~/.local/share/kakehashi
/// - macOS: ~/Library/Application Support/kakehashi
/// - Windows: %APPDATA%/kakehashi
///
/// Note: Returns the base directory only. The resolver functions append
/// "parser/" or "queries/" subdirectories as needed.
pub(crate) fn default_search_paths() -> Vec<String> {
    crate::install::default_data_dir()
        .map(|d| vec![d.to_string_lossy().to_string()])
        .unwrap_or_default()
}

/// Merge multiple RawWorkspaceSettings configs in order.
/// Later configs in the slice have higher precedence (override earlier ones).
/// Use this for layered config: `merge_all(&[defaults, user, project, session])`
pub(crate) fn merge_all(configs: &[Option<RawWorkspaceSettings>]) -> Option<RawWorkspaceSettings> {
    configs.iter().cloned().reduce(merge_settings).flatten()
}

/// Merge two RawWorkspaceSettings, preferring values from `primary` over `fallback`
pub(crate) fn merge_settings(
    fallback: Option<RawWorkspaceSettings>,
    primary: Option<RawWorkspaceSettings>,
) -> Option<RawWorkspaceSettings> {
    match (fallback, primary) {
        (None, None) => None,
        (Some(settings), None) => Some(settings),
        (None, Some(settings)) => Some(settings),
        (Some(fallback), Some(primary)) => {
            let merged = RawWorkspaceSettings {
                // Prefer primary search_paths, fall back to fallback
                search_paths: primary.search_paths.or(fallback.search_paths),

                // Merge languages: start with fallback, override with primary
                languages: merge_languages(fallback.languages, primary.languages),

                // Merge capture mappings: deep merge with primary taking precedence
                capture_mappings: merge_capture_mappings(
                    fallback.capture_mappings,
                    primary.capture_mappings,
                ),

                // Prefer primary auto_install, fall back to fallback
                auto_install: primary.auto_install.or(fallback.auto_install),

                // Deep merge language_servers HashMap
                language_servers: merge_language_servers(
                    fallback.language_servers,
                    primary.language_servers,
                ),
            };
            Some(merged)
        }
    }
}

/// Convert `RawWorkspaceSettings` to `WorkspaceSettings` without expanding
/// environment variables or tilde. This is the base conversion used
/// internally by `try_from_settings`.
fn base_convert(settings: &RawWorkspaceSettings) -> WorkspaceSettings {
    let languages = settings.languages.clone();
    let capture_mappings = settings
        .capture_mappings
        .iter()
        .map(|(lang, mappings)| {
            (
                lang.clone(),
                QueryTypeMappings {
                    highlights: mappings.highlights.clone(),
                    locals: mappings.locals.clone(),
                    folds: mappings.folds.clone(),
                },
            )
        })
        .collect();

    // Use explicit search_paths if provided, otherwise use platform defaults
    let search_paths = settings
        .search_paths
        .clone()
        .unwrap_or_else(default_search_paths);

    WorkspaceSettings {
        search_paths,
        languages,
        capture_mappings,
        auto_install: settings.auto_install.unwrap_or(true),
        language_servers: settings.language_servers.clone().unwrap_or_default(),
    }
}

impl WorkspaceSettings {
    /// Convert `RawWorkspaceSettings` to `WorkspaceSettings`, expanding environment
    /// variables (`$VAR`, `${VAR}`) and tilde (`~`) in path fields.
    ///
    /// Path fields expanded: `search_paths`, `languages[*].parser`, `languages[*].queries[*].path`.
    ///
    /// `home` is the pre-computed home directory (from `dirs::home_dir()`),
    /// passed in so the caller computes it once and tests can inject `None`.
    ///
    /// Uses `base_convert` for the structural conversion, then expands only the
    /// path fields. This avoids duplicating conversion logic.
    pub fn try_from_settings(
        settings: &RawWorkspaceSettings,
        home: Option<&str>,
        env_fn: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, expand::ExpandErrors> {
        let mut ws = base_convert(settings);
        let mut errors = Vec::new();

        for p in &mut ws.search_paths {
            match expand::expand_path(p, home, &env_fn) {
                Ok(expanded) => *p = expanded,
                Err(e) => errors.push(e),
            }
        }

        // Sort keys for deterministic error reporting (HashMap iteration is unordered)
        let mut lang_names: Vec<_> = ws.languages.keys().cloned().collect();
        lang_names.sort();
        for name in lang_names {
            let lang = ws.languages.get_mut(&name).unwrap();
            if let Some(parser) = lang.parser.as_mut() {
                match expand::expand_path(parser, home, &env_fn) {
                    Ok(expanded) => *parser = expanded,
                    Err(e) => errors.push(e),
                }
            }
            if let Some(queries) = lang.queries.as_mut() {
                for q in queries.iter_mut() {
                    match expand::expand_path(&q.path, home, &env_fn) {
                        Ok(expanded) => q.path = expanded,
                        Err(e) => errors.push(e),
                    }
                }
            }
        }

        if errors.is_empty() {
            Ok(ws)
        } else {
            Err(expand::ExpandErrors(errors))
        }
    }
}

impl From<&WorkspaceSettings> for RawWorkspaceSettings {
    fn from(settings: &WorkspaceSettings) -> Self {
        let languages = settings.languages.clone();
        let capture_mappings = settings
            .capture_mappings
            .iter()
            .map(|(lang, mappings)| {
                (
                    lang.clone(),
                    QueryTypeMappings {
                        highlights: mappings.highlights.clone(),
                        locals: mappings.locals.clone(),
                        folds: mappings.folds.clone(),
                    },
                )
            })
            .collect();

        let search_paths = Some(settings.search_paths.clone());

        RawWorkspaceSettings {
            search_paths,
            languages,
            capture_mappings,
            auto_install: Some(settings.auto_install),
            language_servers: Some(settings.language_servers.clone()),
        }
    }
}

impl From<WorkspaceSettings> for RawWorkspaceSettings {
    fn from(settings: WorkspaceSettings) -> Self {
        RawWorkspaceSettings::from(&settings)
    }
}

fn merge_languages(
    mut base: HashMap<String, LanguageSettings>,
    overlay: HashMap<String, LanguageSettings>,
) -> HashMap<String, LanguageSettings> {
    // Deep merge: overlay values override base values for the same key
    for (key, mut overlay_config) in overlay {
        base.entry(key)
            .and_modify(|base_config| {
                base_config.parser = overlay_config.parser.take().or(base_config.parser.take());
                base_config.queries = overlay_config.queries.take().or(base_config.queries.take());
                base_config.bridge = overlay_config.bridge.take().or(base_config.bridge.take());
                base_config.aliases = overlay_config.aliases.take().or(base_config.aliases.take());
            })
            .or_insert(overlay_config);
    }
    base
}

fn merge_language_servers(
    base: Option<HashMap<String, settings::BridgeServerConfig>>,
    overlay: Option<HashMap<String, settings::BridgeServerConfig>>,
) -> Option<HashMap<String, settings::BridgeServerConfig>> {
    match (base, overlay) {
        (None, None) => None,
        (Some(servers), None) | (None, Some(servers)) => Some(servers),
        (Some(mut base_servers), Some(overlay_servers)) => {
            // Deep merge: overlay values override base values for the same key
            for (key, overlay_config) in overlay_servers {
                base_servers
                    .entry(key)
                    .and_modify(|base_config| {
                        // For Vec fields: use overlay if non-empty, otherwise keep base
                        if !overlay_config.cmd.is_empty() {
                            base_config.cmd = overlay_config.cmd.clone();
                        }
                        if !overlay_config.languages.is_empty() {
                            base_config.languages = overlay_config.languages.clone();
                        }
                        // For JSON Option fields: deep merge (ADR-0010)
                        base_config.initialization_options = match (
                            &base_config.initialization_options,
                            &overlay_config.initialization_options,
                        ) {
                            (Some(base_opts), Some(overlay_opts)) => {
                                Some(deep_merge_json(base_opts, overlay_opts))
                            }
                            (Some(base_opts), None) => Some(base_opts.clone()),
                            (None, Some(overlay_opts)) => Some(overlay_opts.clone()),
                            (None, None) => None,
                        };
                        base_config.workspace_type =
                            overlay_config.workspace_type.or(base_config.workspace_type);
                    })
                    .or_insert(overlay_config);
            }
            Some(base_servers)
        }
    }
}

fn merge_capture_mappings(mut base: CaptureMappings, overlay: CaptureMappings) -> CaptureMappings {
    // Deep merge: overlay values override base values for the same key
    for (lang, overlay_mappings) in overlay {
        base.entry(lang)
            .and_modify(|base_mappings| {
                // Merge each mapping type: overlay entries override base entries
                for (k, v) in overlay_mappings.highlights.clone() {
                    base_mappings.highlights.insert(k, v);
                }
                for (k, v) in overlay_mappings.locals.clone() {
                    base_mappings.locals.insert(k, v);
                }
                for (k, v) in overlay_mappings.folds.clone() {
                    base_mappings.folds.insert(k, v);
                }
            })
            .or_insert(overlay_mappings);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn test_merge_settings_with_none() {
        let result = merge_settings(None, None);
        assert!(result.is_none());
    }

    #[test]
    fn test_merge_settings_fallback_only() {
        let fallback = RawWorkspaceSettings {
            search_paths: Some(vec!["/path/to/fallback".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let result = merge_settings(Some(fallback.clone()), None).unwrap();
        assert_eq!(
            result.search_paths,
            Some(vec!["/path/to/fallback".to_string()])
        );
    }

    #[test]
    fn test_merge_settings_primary_only() {
        let primary = RawWorkspaceSettings {
            search_paths: Some(vec!["/path/to/primary".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let result = merge_settings(None, Some(primary.clone())).unwrap();
        assert_eq!(
            result.search_paths,
            Some(vec!["/path/to/primary".to_string()])
        );
    }

    #[test]
    fn test_merge_settings_prefer_primary() {
        let mut fallback_languages = HashMap::new();
        fallback_languages.insert(
            "rust".to_string(),
            LanguageSettings {
                parser: Some("/fallback/rust.so".to_string()),
                ..Default::default()
            },
        );

        let fallback = RawWorkspaceSettings {
            search_paths: Some(vec!["/path/to/fallback".to_string()]),
            languages: fallback_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let mut primary_languages = HashMap::new();
        primary_languages.insert(
            "rust".to_string(),
            LanguageSettings {
                parser: Some("/primary/rust.so".to_string()),
                ..Default::default()
            },
        );

        let primary = RawWorkspaceSettings {
            search_paths: Some(vec!["/path/to/primary".to_string()]),
            languages: primary_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(fallback), Some(primary)).unwrap();

        // Primary search paths should win
        assert_eq!(
            result.search_paths,
            Some(vec!["/path/to/primary".to_string()])
        );

        // Primary language config should override fallback
        assert_eq!(
            result.languages["rust"].parser,
            Some("/primary/rust.so".to_string())
        );
    }

    #[test]
    fn test_merge_capture_mappings() {
        let mut fallback_mappings = HashMap::new();
        let mut fallback_highlights = HashMap::new();
        fallback_highlights.insert(
            "variable.builtin".to_string(),
            "fallback.variable".to_string(),
        );
        fallback_highlights.insert(
            "function.builtin".to_string(),
            "fallback.function".to_string(),
        );

        fallback_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: fallback_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        let fallback = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: fallback_mappings,
            auto_install: None,
            language_servers: None,
        };

        let mut primary_mappings = HashMap::new();
        let mut primary_highlights = HashMap::new();
        primary_highlights.insert(
            "variable.builtin".to_string(),
            "primary.variable".to_string(),
        );
        primary_highlights.insert("type.builtin".to_string(), "primary.type".to_string());

        primary_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: primary_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        let primary = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: primary_mappings,
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(fallback), Some(primary)).unwrap();

        // Primary should override fallback for same keys
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].highlights["variable.builtin"],
            "primary.variable"
        );

        // Primary adds new keys
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].highlights["type.builtin"],
            "primary.type"
        );

        // Fallback keys not in primary should remain
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].highlights["function.builtin"],
            "fallback.function"
        );
    }

    #[test]
    fn test_capture_mapping_handles_at_prefix() {
        // Create capture mappings with "@" prefix
        let mut capture_mappings = CaptureMappings::new();

        let mut highlights = HashMap::new();
        highlights.insert("@module".to_string(), "@namespace".to_string());
        highlights.insert(
            "@module.builtin".to_string(),
            "@namespace.defaultLibrary".to_string(),
        );

        let query_type_mappings = QueryTypeMappings {
            highlights,
            locals: HashMap::new(),
            folds: HashMap::new(),
        };

        capture_mappings.insert(WILDCARD_KEY.to_string(), query_type_mappings);

        // Verify the mapping exists and contains expected values
        assert!(capture_mappings.contains_key(WILDCARD_KEY));
        let wildcard_mappings = capture_mappings.get(WILDCARD_KEY).unwrap();
        assert_eq!(
            wildcard_mappings.highlights.get("@module"),
            Some(&"@namespace".to_string())
        );
        assert_eq!(
            wildcard_mappings.highlights.get("@module.builtin"),
            Some(&"@namespace.defaultLibrary".to_string())
        );
    }

    #[test]
    fn test_default_search_paths_used_when_none_configured() {
        // When search_paths is None in RawWorkspaceSettings, WorkspaceSettings
        // should use the default data directory paths (not an empty vector)
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);

        // Default paths should be populated (not empty)
        assert!(
            !workspace.search_paths.is_empty(),
            "search_paths should contain default data directory paths when not configured"
        );

        // Should contain parser and queries subdirectories
        let paths_str = workspace.search_paths.join("|");
        assert!(
            paths_str.contains("kakehashi"),
            "Default paths should include kakehashi directory: {:?}",
            workspace.search_paths
        );
    }

    #[test]
    fn test_explicit_search_paths_override_default() {
        // When search_paths is explicitly set, it should be used as-is
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["/custom/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);

        // Should use explicit paths, not default
        assert_eq!(workspace.search_paths, vec!["/custom/path".to_string()]);
    }

    #[test]
    fn test_search_paths_can_include_default() {
        // Users can extend default paths by including them explicitly
        let default_paths = default_search_paths();
        let mut paths = vec!["/custom/path".to_string()];
        paths.extend(default_paths.clone());

        let settings = RawWorkspaceSettings {
            search_paths: Some(paths.clone()),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);

        // Should use the combined paths
        assert_eq!(workspace.search_paths.len(), 2); // 1 custom + 1 default (base dir only)
        assert_eq!(workspace.search_paths[0], "/custom/path");
        // Default paths follow
        for (i, default_path) in default_paths.iter().enumerate() {
            assert_eq!(&workspace.search_paths[i + 1], default_path);
        }
    }

    #[rstest]
    #[case::default_true(None, true)]
    #[case::explicit_true(Some(true), true)]
    #[case::explicit_false(Some(false), false)]
    fn test_auto_install(#[case] auto_install: Option<bool>, #[case] expected: bool) {
        // PBI-019: autoInstall defaults to true for zero-config; explicit values honored
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install,
            language_servers: None,
        };

        let workspace: WorkspaceSettings = base_convert(&settings);
        assert_eq!(workspace.auto_install, expected);
    }

    #[test]
    fn test_default_search_paths_format() {
        // PBI-028: default_search_paths() should return base directory only.
        //
        // resolve_library_path() appends "parser/" to each search path,
        // so default_search_paths() should NOT include "parser" or "queries" subdirectories.
        //
        // WRONG: [".../kakehashi/parser", ".../kakehashi/queries"]
        //   -> resolve_library_path looks for ".../kakehashi/parser/parser/lua.so" (FAILS)
        //
        // CORRECT: [".../kakehashi"]
        //   -> resolve_library_path looks for ".../kakehashi/parser/lua.so" (WORKS)
        let paths = default_search_paths();

        // Should have exactly one path (the base directory)
        assert_eq!(
            paths.len(),
            1,
            "default_search_paths should return single base directory, got {:?}",
            paths
        );

        // The path should NOT end with "/parser" or "/queries"
        let path = &paths[0];
        assert!(
            !path.ends_with("/parser") && !path.ends_with("/queries"),
            "Path should be base directory, not subdirectory: {}",
            path
        );

        // The path should end with "kakehashi" (the base directory name)
        assert!(
            path.ends_with("kakehashi"),
            "Path should end with 'kakehashi': {}",
            path
        );
    }

    // PBI-150: merge_all() tests for multi-layer config merging

    #[test]
    fn test_merge_all_empty_slice_returns_none() {
        // Empty config slice should return None
        let result = merge_all(&[]);
        assert!(result.is_none());
    }

    #[test]
    fn test_merge_all_single_some_returns_it() {
        // Single Some config should return that config
        let config = RawWorkspaceSettings {
            search_paths: Some(vec!["/path/one".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(true),
            language_servers: None,
        };
        let result = merge_all(&[Some(config.clone())]);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.search_paths, Some(vec!["/path/one".to_string()]));
        assert_eq!(result.auto_install, Some(true));
    }

    #[test]
    fn test_merge_settings_scalar_later_wins() {
        // Later config's scalar values should override earlier ones
        // Simulates: user config has autoInstall=true, project has autoInstall=false
        let user_config = RawWorkspaceSettings {
            search_paths: Some(vec!["/user/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(true),
            language_servers: None,
        };
        let project_config = RawWorkspaceSettings {
            search_paths: Some(vec!["/project/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(false),
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // Project's values should win (later overrides earlier)
        assert_eq!(result.search_paths, Some(vec!["/project/path".to_string()]));
        assert_eq!(result.auto_install, Some(false));
    }

    #[test]
    fn test_merge_all_four_layers() {
        // Test the full 4-layer merge: defaults < user < project < session
        let defaults = RawWorkspaceSettings {
            search_paths: Some(vec!["/default/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(true),
            language_servers: None,
        };
        let user_config = RawWorkspaceSettings {
            search_paths: None, // Not overriding, should inherit from defaults
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(true),
            language_servers: None,
        };
        let project_config = RawWorkspaceSettings {
            search_paths: Some(vec!["/project/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None, // Not overriding, should inherit
            language_servers: None,
        };
        let session_config = RawWorkspaceSettings {
            search_paths: None, // Not overriding
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(false), // Session wins
            language_servers: None,
        };

        let result = merge_all(&[
            Some(defaults),
            Some(user_config),
            Some(project_config),
            Some(session_config),
        ]);

        assert!(result.is_some());
        let result = result.unwrap();

        // search_paths: project wins (later non-None override)
        assert_eq!(result.search_paths, Some(vec!["/project/path".to_string()]));
        // auto_install: session wins
        assert_eq!(result.auto_install, Some(false));
    }

    #[test]
    fn test_merge_all_skips_none_configs() {
        // None configs in the slice should be skipped
        let config = RawWorkspaceSettings {
            search_paths: Some(vec!["/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: Some(true),
            language_servers: None,
        };

        let result = merge_all(&[None, Some(config.clone()), None]);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.search_paths, Some(vec!["/path".to_string()]));
    }

    // PBI-150 Subtask 2: Deep merge for languages HashMap

    #[test]
    fn test_merge_settings_languages_deep_merge() {
        // Project sets queries field, inherits parser and bridge from user config
        // This is the key behavior change from shallow to deep merge
        use settings::BridgeLanguageConfig;

        let mut user_languages = HashMap::new();
        let mut user_bridge = HashMap::new();
        user_bridge.insert(
            "rust".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );

        user_languages.insert(
            "python".to_string(),
            LanguageSettings {
                parser: Some("/usr/lib/python.so".to_string()),
                queries: Some(vec![
                    settings::QueryItem {
                        path: "/usr/share/python/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    },
                    settings::QueryItem {
                        path: "/usr/share/python/locals.scm".to_string(),
                        kind: Some(settings::QueryKind::Locals),
                    },
                ]),
                bridge: Some(user_bridge),
                ..Default::default()
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: user_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        // Project overrides queries for python
        let mut project_languages = HashMap::new();
        project_languages.insert(
            "python".to_string(),
            LanguageSettings {
                // parser: None - Not specified - should inherit from user
                queries: Some(vec![settings::QueryItem {
                    path: "./queries/python-highlights.scm".to_string(),
                    kind: Some(settings::QueryKind::Highlights),
                }]),
                // bridge: None - Not specified - should inherit from user
                ..Default::default()
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: project_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // Python should exist
        assert!(result.languages.contains_key("python"));
        let python = &result.languages["python"];

        // Library: inherited from user (project was None)
        assert_eq!(python.parser, Some("/usr/lib/python.so".to_string()));

        // Queries: overridden by project (array replacement, not merge)
        let queries = python.queries.as_ref().unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].path, "./queries/python-highlights.scm");

        // Bridge: inherited from user (project was None)
        assert!(python.bridge.is_some());
        let bridge = python.bridge.as_ref().unwrap();
        assert_eq!(bridge.get("rust").unwrap().enabled, Some(true));
    }

    #[test]
    fn test_merge_settings_languages_adds_new_keys() {
        // User has python, project adds rust - both should exist
        let mut user_languages = HashMap::new();
        user_languages.insert(
            "python".to_string(),
            LanguageSettings {
                parser: Some("/usr/lib/python.so".to_string()),
                ..Default::default()
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: user_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let mut project_languages = HashMap::new();
        project_languages.insert(
            "rust".to_string(),
            LanguageSettings {
                parser: Some("/project/rust.so".to_string()),
                ..Default::default()
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: project_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // Both languages should exist
        assert!(result.languages.contains_key("python"));
        assert!(result.languages.contains_key("rust"));

        // Python from user
        assert_eq!(
            result.languages["python"].parser,
            Some("/usr/lib/python.so".to_string())
        );

        // Rust from project
        assert_eq!(
            result.languages["rust"].parser,
            Some("/project/rust.so".to_string())
        );
    }

    // PBI-150 Subtask 3: Deep merge for languageServers HashMap

    #[test]
    fn test_merge_settings_language_servers_deep_merge() {
        // Project adds initializationOptions to rust-analyzer, inherits cmd and languages from user
        use serde_json::json;
        use settings::{BridgeServerConfig, WorkspaceType};

        let mut user_servers = HashMap::new();
        user_servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_type: Some(WorkspaceType::Cargo),
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: Some(user_servers),
        };

        // Project only adds initializationOptions
        let mut project_servers = HashMap::new();
        project_servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec![],       // Empty, should inherit from user
                languages: vec![], // Empty, should inherit from user
                initialization_options: Some(json!({ "linkedProjects": ["./Cargo.toml"] })),
                workspace_type: None, // Should inherit from user
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: Some(project_servers),
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        assert!(result.language_servers.is_some());
        let servers = result.language_servers.as_ref().unwrap();
        assert!(servers.contains_key("rust-analyzer"));

        let ra = &servers["rust-analyzer"];

        // cmd: inherited from user (project was empty)
        assert_eq!(ra.cmd, vec!["rust-analyzer".to_string()]);

        // languages: inherited from user (project was empty)
        assert_eq!(ra.languages, vec!["rust".to_string()]);

        // initializationOptions: added by project
        assert!(ra.initialization_options.is_some());
        let init_opts = ra.initialization_options.as_ref().unwrap();
        assert!(init_opts.get("linkedProjects").is_some());

        // workspaceType: inherited from user
        assert_eq!(ra.workspace_type, Some(WorkspaceType::Cargo));
    }

    #[test]
    fn test_merge_settings_language_servers_adds_new_server() {
        // User has rust-analyzer, project adds pyright - both should exist
        use settings::BridgeServerConfig;

        let mut user_servers = HashMap::new();
        user_servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: Some(user_servers),
        };

        let mut project_servers = HashMap::new();
        project_servers.insert(
            "pyright".to_string(),
            BridgeServerConfig {
                cmd: vec!["pyright-langserver".to_string(), "--stdio".to_string()],
                languages: vec!["python".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: Some(project_servers),
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        assert!(result.language_servers.is_some());
        let servers = result.language_servers.as_ref().unwrap();

        // Both servers should exist
        assert!(servers.contains_key("rust-analyzer"));
        assert!(servers.contains_key("pyright"));

        // rust-analyzer from user
        assert_eq!(
            servers["rust-analyzer"].cmd,
            vec!["rust-analyzer".to_string()]
        );

        // pyright from project
        assert_eq!(
            servers["pyright"].cmd,
            vec!["pyright-langserver".to_string(), "--stdio".to_string()]
        );
    }

    // PBI-150 Subtask 4: Deep merge for captureMappings (already implemented, verify via merge_all)

    #[test]
    fn test_merge_settings_capture_mappings_deep_merge() {
        // Project overrides variable.builtin, inherits function.builtin from user config
        let mut user_mappings = HashMap::new();
        let mut user_highlights = HashMap::new();
        user_highlights.insert(
            "variable.builtin".to_string(),
            "variable.defaultLibrary".to_string(),
        );
        user_highlights.insert(
            "function.builtin".to_string(),
            "function.defaultLibrary".to_string(),
        );

        user_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: user_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: user_mappings,
            auto_install: None,
            language_servers: None,
        };

        // Project only overrides variable.builtin
        let mut project_mappings = HashMap::new();
        let mut project_highlights = HashMap::new();
        project_highlights.insert(
            "variable.builtin".to_string(),
            "project.variable".to_string(),
        );

        project_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: project_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: project_mappings,
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // variable.builtin: overridden by project
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].highlights["variable.builtin"],
            "project.variable"
        );

        // function.builtin: inherited from user
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].highlights["function.builtin"],
            "function.defaultLibrary"
        );
    }

    #[test]
    fn test_merge_settings_capture_mappings_adds_new_language() {
        // User has wildcard "_", project adds "rust" - both should exist
        let mut user_mappings = HashMap::new();
        let mut user_highlights = HashMap::new();
        user_highlights.insert("variable.builtin".to_string(), "user.variable".to_string());

        user_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: user_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: user_mappings,
            auto_install: None,
            language_servers: None,
        };

        let mut project_mappings = HashMap::new();
        let mut rust_highlights = HashMap::new();
        rust_highlights.insert("type.builtin".to_string(), "rust.type".to_string());

        project_mappings.insert(
            "rust".to_string(),
            QueryTypeMappings {
                highlights: rust_highlights,
                locals: HashMap::new(),
                folds: HashMap::new(),
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: project_mappings,
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // Both language keys should exist
        assert!(result.capture_mappings.contains_key(WILDCARD_KEY));
        assert!(result.capture_mappings.contains_key("rust"));

        // Wildcard from user
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].highlights["variable.builtin"],
            "user.variable"
        );

        // Rust from project
        assert_eq!(
            result.capture_mappings["rust"].highlights["type.builtin"],
            "rust.type"
        );
    }

    #[test]
    fn test_merge_settings_capture_mappings_locals_and_folds() {
        // Verify deep merge works for locals and folds, not just highlights
        let mut user_mappings = HashMap::new();
        let mut user_locals = HashMap::new();
        user_locals.insert(
            "definition.var".to_string(),
            "definition.variable".to_string(),
        );
        let mut user_folds = HashMap::new();
        user_folds.insert("fold.comment".to_string(), "comment".to_string());

        user_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: HashMap::new(),
                locals: user_locals,
                folds: user_folds,
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: user_mappings,
            auto_install: None,
            language_servers: None,
        };

        // Project overrides one locals, adds one folds
        let mut project_mappings = HashMap::new();
        let mut project_locals = HashMap::new();
        project_locals.insert(
            "definition.var".to_string(),
            "project.definition".to_string(),
        );
        let mut project_folds = HashMap::new();
        project_folds.insert("fold.function".to_string(), "function".to_string());

        project_mappings.insert(
            "_".to_string(),
            QueryTypeMappings {
                highlights: HashMap::new(),
                locals: project_locals,
                folds: project_folds,
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: HashMap::new(),
            capture_mappings: project_mappings,
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // locals.definition.var: overridden by project
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].locals["definition.var"],
            "project.definition"
        );

        // folds.fold.comment: inherited from user
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].folds["fold.comment"],
            "comment"
        );

        // folds.fold.function: added by project
        assert_eq!(
            result.capture_mappings[WILDCARD_KEY].folds["fold.function"],
            "function"
        );
    }

    // PBI-153: Languages Wildcard Inheritance (ADR-0011)

    #[test]
    fn test_specific_values_override_wildcards_at_both_levels() {
        // ADR-0011: python.bridge.javascript overrides _.bridge._ settings
        // Setup:
        // - languages._ has bridge._ with enabled = true (default)
        // - languages.python has bridge.javascript with enabled = false (override)
        // - We ask for bridge setting for "javascript" in "python" -> should get enabled = false
        // - We ask for bridge setting for "rust" in "python" -> should get enabled = true (from _)
        let mut languages: HashMap<String, LanguageSettings> = HashMap::new();

        // Wildcard language with wildcard bridge (default enabled = true)
        let mut wildcard_bridge = HashMap::new();
        wildcard_bridge.insert(
            "_".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );

        languages.insert(
            "_".to_string(),
            LanguageSettings {
                parser: Some("/default/path.so".to_string()),
                queries: Some(vec![settings::QueryItem {
                    path: "/default/highlights.scm".to_string(),
                    kind: Some(settings::QueryKind::Highlights),
                }]),
                bridge: Some(wildcard_bridge),
                ..Default::default()
            },
        );

        // Python-specific: disable bridging to JavaScript, but inherit _ for parser
        let mut python_bridge = HashMap::new();
        python_bridge.insert(
            "javascript".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(false),
                ..Default::default()
            },
        );

        languages.insert(
            "python".to_string(),
            LanguageSettings {
                // parser: None - Should inherit from _
                // queries: None - Should inherit from _
                bridge: Some(python_bridge),
                ..Default::default()
            },
        );

        // Resolve for "python" - should merge with wildcard
        let resolved_lang = resolve_with_wildcard(&languages, "python", merge_language_settings);
        assert!(resolved_lang.is_some(), "Should resolve python language");
        let lang_config = resolved_lang.unwrap();

        // Parser should be inherited from wildcard
        assert_eq!(
            lang_config.parser,
            Some("/default/path.so".to_string()),
            "Python should inherit parser from wildcard"
        );

        // Bridge should be deep merged: wildcard + python-specific
        assert!(lang_config.bridge.is_some(), "Python should have bridge");
        let bridge = lang_config.bridge.as_ref().unwrap();

        // JavaScript: python-specific override (enabled = false)
        let js_resolved =
            resolve_with_wildcard(bridge, "javascript", merge_bridge_language_configs);
        assert!(js_resolved.is_some(), "Should resolve javascript bridge");
        assert!(
            js_resolved.unwrap().enabled == Some(false),
            "Python's javascript bridge should be disabled (override)"
        );

        // Rust: inherited from _.bridge._ through deep merge
        // ADR-0011: bridge maps are deep merged, so python gets wildcard's bridge._
        let rust_resolved = resolve_with_wildcard(bridge, "rust", merge_bridge_language_configs);
        assert!(
            rust_resolved.is_some(),
            "Python's rust bridge should resolve (inherited from wildcard's bridge._)"
        );
        assert!(
            rust_resolved.unwrap().enabled == Some(true),
            "Python's rust bridge should be enabled (from wildcard's bridge._)"
        );
    }

    #[test]
    fn test_specific_bridge_with_nested_wildcard() {
        // ADR-0011: Test case where python.bridge includes _ wildcard
        // - languages.python.bridge._ = enabled: true (python-specific default)
        // - languages.python.bridge.javascript = enabled: false (override)
        // - rust should inherit from python.bridge._ (enabled = true)
        let mut languages: HashMap<String, LanguageSettings> = HashMap::new();

        // Python with its own wildcard bridge
        let mut python_bridge = HashMap::new();
        python_bridge.insert(
            "_".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            }, // Python's own default
        );
        python_bridge.insert(
            "javascript".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(false),
                ..Default::default()
            }, // Override for JS
        );

        languages.insert(
            "python".to_string(),
            LanguageSettings {
                parser: Some("/python/path.so".to_string()),
                bridge: Some(python_bridge),
                ..Default::default()
            },
        );

        let resolved_lang = resolve_with_wildcard(&languages, "python", merge_language_settings);
        assert!(resolved_lang.is_some());
        let lang_config = resolved_lang.unwrap();
        let bridge = lang_config.bridge.as_ref().unwrap();

        // JavaScript: specific override
        let js_resolved =
            resolve_with_wildcard(bridge, "javascript", merge_bridge_language_configs);
        assert!(js_resolved.is_some());
        assert!(
            js_resolved.unwrap().enabled == Some(false),
            "JavaScript should be disabled"
        );

        // Rust: inherits from python's bridge._
        let rust_resolved = resolve_with_wildcard(bridge, "rust", merge_bridge_language_configs);
        assert!(rust_resolved.is_some());
        assert!(
            rust_resolved.unwrap().enabled == Some(true),
            "Rust should inherit from python.bridge._"
        );
    }

    #[test]
    fn test_nested_wildcard_resolution_outer_then_inner() {
        // ADR-0011: Nested wildcard resolution applies outer then inner
        // Resolution order:
        // 1. Resolve outer: languages._ -> languages.python
        // 2. Resolve inner: bridge._ -> bridge.rust
        //
        // Setup:
        // languages._ has bridge._ with enabled = true
        // languages.python is NOT defined (should inherit from _)
        // We ask for bridge setting for "rust" in "python" -> should get enabled = true
        let mut languages: HashMap<String, LanguageSettings> = HashMap::new();

        // Wildcard language with wildcard bridge
        let mut wildcard_bridge = HashMap::new();
        wildcard_bridge.insert(
            "_".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );

        languages.insert(
            "_".to_string(),
            LanguageSettings {
                parser: Some("/default/path.so".to_string()),
                bridge: Some(wildcard_bridge),
                ..Default::default()
            },
        );

        // Resolve for "python" which doesn't exist - should get wildcard language
        let resolved_lang = resolve_with_wildcard(&languages, "python", merge_language_settings);
        assert!(
            resolved_lang.is_some(),
            "Should resolve to wildcard language"
        );

        // Then resolve bridge for "rust" within the resolved language
        let lang_config = resolved_lang.unwrap();
        assert!(
            lang_config.bridge.is_some(),
            "Resolved language should have bridge"
        );
        let bridge = lang_config.bridge.as_ref().unwrap();

        let resolved_bridge = resolve_with_wildcard(bridge, "rust", merge_bridge_language_configs);
        assert!(
            resolved_bridge.is_some(),
            "Should resolve to wildcard bridge"
        );
        assert!(
            resolved_bridge.unwrap().enabled == Some(true),
            "Nested wildcard resolution: languages._.bridge._ should apply to python.bridge.rust"
        );
    }

    #[test]
    fn test_resolve_language_with_wildcard_deep_merges_bridge_maps() {
        // ADR-0011: Bridge maps should be deep merged, not overridden
        //
        // Setup:
        // - languages._.bridge = { rust: enabled=true, go: enabled=true }
        // - languages.python.bridge = { javascript: enabled=false }
        //
        // Expected after merge:
        // - languages.python.bridge = { rust: true, go: true, javascript: false }
        //
        // This tests that python inherits rust/go from wildcard while adding
        // its own javascript setting.
        let mut languages: HashMap<String, LanguageSettings> = HashMap::new();

        // Wildcard has default bridge settings for rust and go
        let mut wildcard_bridge = HashMap::new();
        wildcard_bridge.insert(
            "rust".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        wildcard_bridge.insert(
            "go".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );

        languages.insert(
            "_".to_string(),
            LanguageSettings {
                parser: Some("/default/path.so".to_string()),
                bridge: Some(wildcard_bridge),
                ..Default::default()
            },
        );

        // Python adds javascript bridge but should inherit rust/go from wildcard
        let mut python_bridge = HashMap::new();
        python_bridge.insert(
            "javascript".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(false),
                ..Default::default()
            },
        );

        languages.insert(
            "python".to_string(),
            LanguageSettings {
                // parser: None - Inherits from wildcard
                bridge: Some(python_bridge),
                ..Default::default()
            },
        );

        // Resolve for "python" - bridge should be deep merged with wildcard
        let resolved = resolve_with_wildcard(&languages, "python", merge_language_settings);
        assert!(resolved.is_some());
        let lang_config = resolved.unwrap();

        // Parser should be inherited from wildcard
        assert_eq!(
            lang_config.parser,
            Some("/default/path.so".to_string()),
            "Python should inherit parser from wildcard"
        );

        // Bridge should be deep merged
        assert!(lang_config.bridge.is_some(), "Python should have bridge");
        let bridge = lang_config.bridge.as_ref().unwrap();

        // rust: inherited from wildcard
        assert!(
            bridge.get("rust").is_some_and(|c| c.enabled == Some(true)),
            "Python should inherit rust bridge from wildcard (deep merge)"
        );

        // go: inherited from wildcard
        assert!(
            bridge.get("go").is_some_and(|c| c.enabled == Some(true)),
            "Python should inherit go bridge from wildcard (deep merge)"
        );

        // javascript: python-specific
        assert!(
            bridge
                .get("javascript")
                .is_some_and(|c| c.enabled == Some(false)),
            "Python should have its own javascript bridge setting"
        );
    }

    // PBI-154: languageServers Wildcard Inheritance (ADR-0011)

    /// Helper to build servers map for wildcard resolution tests
    fn build_servers_map(
        wildcard: Option<settings::BridgeServerConfig>,
        specific: Option<settings::BridgeServerConfig>,
    ) -> HashMap<String, settings::BridgeServerConfig> {
        let mut servers = HashMap::new();
        if let Some(w) = wildcard {
            servers.insert("_".to_string(), w);
        }
        if let Some(s) = specific {
            servers.insert("rust-analyzer".to_string(), s);
        }
        servers
    }

    #[test]
    fn test_resolve_language_server_returns_none_when_neither_exists() {
        // ADR-0011: Neither wildcard nor specific key -> return None
        let servers = build_servers_map(None, None);

        let result = resolve_with_wildcard(&servers, "rust-analyzer", merge_bridge_server_configs);

        assert!(
            result.is_none(),
            "Should return None when neither wildcard nor specific key exists"
        );
    }

    #[test]
    fn test_resolve_language_server_returns_wildcard_when_specific_absent() {
        // ADR-0011: When languageServers only has "_" and we ask for "rust-analyzer",
        // we should get the wildcard's settings
        let wildcard = settings::BridgeServerConfig {
            cmd: vec!["default-lsp".to_string()],
            languages: vec!["any".to_string()],
            initialization_options: None,
            workspace_type: Some(settings::WorkspaceType::Generic),
        };
        let servers = build_servers_map(Some(wildcard), None);

        let result = resolve_with_wildcard(&servers, "rust-analyzer", merge_bridge_server_configs);

        assert!(result.is_some(), "Should return Some when wildcard exists");
        let resolved = result.unwrap();
        assert_eq!(
            resolved.cmd,
            vec!["default-lsp".to_string()],
            "Should inherit cmd from wildcard"
        );
        assert_eq!(
            resolved.languages,
            vec!["any".to_string()],
            "Should inherit languages from wildcard"
        );
        assert_eq!(
            resolved.workspace_type,
            Some(settings::WorkspaceType::Generic),
            "Should inherit workspace_type from wildcard"
        );
    }

    #[test]
    fn test_resolve_language_server_returns_specific_when_wildcard_absent() {
        // ADR-0011: No wildcard, only specific key -> return specific
        let specific = settings::BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec!["rust".to_string()],
            initialization_options: None,
            workspace_type: Some(settings::WorkspaceType::Cargo),
        };
        let servers = build_servers_map(None, Some(specific));

        let result = resolve_with_wildcard(&servers, "rust-analyzer", merge_bridge_server_configs);

        assert!(
            result.is_some(),
            "Should return Some when specific key exists"
        );
        let resolved = result.unwrap();
        assert_eq!(
            resolved.cmd,
            vec!["rust-analyzer".to_string()],
            "Should return specific config"
        );
        assert_eq!(
            resolved.languages,
            vec!["rust".to_string()],
            "Should return specific languages"
        );
        assert_eq!(
            resolved.workspace_type,
            Some(settings::WorkspaceType::Cargo),
            "Should return specific workspace_type"
        );
    }

    #[test]
    fn test_resolve_language_server_specific_overrides_wildcard() {
        // ADR-0011: Server-specific values override wildcard values
        // When both wildcard and specific server exist, specific values take precedence
        use serde_json::json;

        let wildcard = settings::BridgeServerConfig {
            cmd: vec!["default-lsp".to_string()],
            languages: vec!["any".to_string()],
            initialization_options: Some(json!({ "defaultOption": true })),
            workspace_type: Some(settings::WorkspaceType::Generic),
        };
        let specific = settings::BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec![], // Empty means inherit from wildcard
            initialization_options: Some(json!({ "linkedProjects": ["./Cargo.toml"] })),
            workspace_type: Some(settings::WorkspaceType::Cargo),
        };
        let servers = build_servers_map(Some(wildcard), Some(specific));

        let result = resolve_with_wildcard(&servers, "rust-analyzer", merge_bridge_server_configs);

        assert!(result.is_some(), "Should return merged config");
        let resolved = result.unwrap();

        // cmd: overridden by specific
        assert_eq!(
            resolved.cmd,
            vec!["rust-analyzer".to_string()],
            "Should use rust-analyzer's cmd"
        );

        // languages: inherited from wildcard (specific was empty)
        assert_eq!(
            resolved.languages,
            vec!["any".to_string()],
            "Should inherit languages from wildcard since specific is empty"
        );

        // workspace_type: overridden by specific
        assert_eq!(
            resolved.workspace_type,
            Some(settings::WorkspaceType::Cargo),
            "Should use rust-analyzer's workspace_type"
        );

        // initialization_options: deep merged (ADR-0010)
        let init_opts = resolved
            .initialization_options
            .expect("Should have merged initialization_options");
        assert_eq!(
            init_opts.get("linkedProjects"),
            Some(&json!(["./Cargo.toml"])),
            "Should have rust-analyzer's linkedProjects"
        );
        assert_eq!(
            init_opts.get("defaultOption"),
            Some(&json!(true)),
            "Should inherit defaultOption from wildcard (deep merge per ADR-0010)"
        );
    }

    // PBI-157: Deep merge for initialization_options (ADR-0010)

    #[test]
    fn test_merge_bridge_server_configs_deep_merges_initialization_options() {
        // ADR-0010: initialization_options should be deep merged, not replaced
        // Base provides {feature1: true}, overlay provides {feature2: true}
        // Result should be {feature1: true, feature2: true}
        use serde_json::json;
        use settings::BridgeServerConfig;

        let base = BridgeServerConfig {
            cmd: vec!["default-lsp".to_string()],
            languages: vec![],
            initialization_options: Some(json!({ "feature1": true })),
            workspace_type: None,
        };
        let overlay = BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec!["rust".to_string()],
            initialization_options: Some(json!({ "feature2": true })),
            workspace_type: None,
        };

        let resolved = merge_bridge_server_configs(&base, &overlay);

        // Should deep merge both features
        let init_opts = resolved.initialization_options.unwrap();
        assert_eq!(
            init_opts.get("feature1"),
            Some(&json!(true)),
            "Should inherit feature1 from base (deep merge)"
        );
        assert_eq!(
            init_opts.get("feature2"),
            Some(&json!(true)),
            "Should have feature2 from overlay"
        );
    }

    #[test]
    fn test_merge_language_servers_deep_merges_initialization_options() {
        // ADR-0010: merge_language_servers should deep merge initialization_options
        // Base layer has {baseOpt: 1}, overlay has {overlayOpt: 2}
        // Result should have both options
        use serde_json::json;
        use settings::BridgeServerConfig;

        let mut base_servers = HashMap::new();
        base_servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: Some(json!({ "baseOpt": 1 })),
                workspace_type: None,
            },
        );

        let mut overlay_servers = HashMap::new();
        overlay_servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec![],
                languages: vec![],
                initialization_options: Some(json!({ "overlayOpt": 2 })),
                workspace_type: None,
            },
        );

        let result = merge_language_servers(Some(base_servers), Some(overlay_servers));
        assert!(result.is_some());
        let merged = result.unwrap();
        assert!(merged.contains_key("rust-analyzer"));

        let ra = &merged["rust-analyzer"];
        let init_opts = ra.initialization_options.as_ref().unwrap();

        // Should have both options (deep merge)
        assert_eq!(
            init_opts.get("baseOpt"),
            Some(&json!(1)),
            "Should preserve baseOpt from base layer"
        );
        assert_eq!(
            init_opts.get("overlayOpt"),
            Some(&json!(2)),
            "Should have overlayOpt from overlay layer"
        );
    }

    #[test]
    fn test_merge_bridge_server_configs_overlay_overrides_base_same_key() {
        // ADR-0010: Overlay values override base for same keys
        // Base has {opt: 1}, overlay has {opt: 2}
        // Result should be {opt: 2}
        use serde_json::json;
        use settings::BridgeServerConfig;

        let base = BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: Some(json!({ "opt": 1 })),
            workspace_type: None,
        };
        let overlay = BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec![],
            initialization_options: Some(json!({ "opt": 2 })),
            workspace_type: None,
        };

        let resolved = merge_bridge_server_configs(&base, &overlay);
        let init_opts = resolved.initialization_options.unwrap();

        // Overlay value should override base
        assert_eq!(
            init_opts.get("opt"),
            Some(&json!(2)),
            "Overlay value should override base for same key"
        );
    }

    #[test]
    fn test_merge_bridge_server_configs_nested_objects_deep_merge() {
        // ADR-0010: Nested JSON objects should merge recursively
        // Base has {a: {b: 1}}, overlay has {a: {c: 2}}
        // Result should be {a: {b: 1, c: 2}}
        use serde_json::json;
        use settings::BridgeServerConfig;

        let base = BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: Some(json!({ "a": { "b": 1 } })),
            workspace_type: None,
        };
        let overlay = BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec![],
            initialization_options: Some(json!({ "a": { "c": 2 } })),
            workspace_type: None,
        };

        let resolved = merge_bridge_server_configs(&base, &overlay);
        let init_opts = resolved.initialization_options.unwrap();

        // Should deep merge nested objects
        let a_obj = init_opts.get("a").unwrap().as_object().unwrap();
        assert_eq!(
            a_obj.get("b"),
            Some(&json!(1)),
            "Should preserve b from base"
        );
        assert_eq!(
            a_obj.get("c"),
            Some(&json!(2)),
            "Should have c from overlay"
        );
    }

    // ========================================================================
    // Tests moved from lsp_impl.rs (Phase 6.1)
    // These test wildcard config resolution functions
    // ========================================================================

    /// PBI-155 Subtask 2: Test wildcard language config inheritance
    ///
    /// This test verifies that languages._ (wildcard) settings are inherited
    /// by specific languages when looking up language configs.
    ///
    /// The key behavior:
    /// - languages._ defines default bridge settings (e.g., disable all by default)
    /// - languages.markdown overrides only bridge for rust (enable it)
    /// - When looking up "quarto" (not defined), it should inherit from languages._
    #[test]
    fn test_language_config_inherits_from_wildcard() {
        use settings::BridgeLanguageConfig;

        let mut languages: HashMap<String, LanguageSettings> = HashMap::new();

        // Wildcard language: disable bridging by default (empty bridge filter)
        let wildcard_bridge = HashMap::new(); // Empty = disable all bridging
        languages.insert(
            "_".to_string(),
            LanguageSettings {
                queries: Some(vec![settings::QueryItem {
                    path: "/default/highlights.scm".to_string(),
                    kind: Some(settings::QueryKind::Highlights),
                }]),
                bridge: Some(wildcard_bridge),
                ..Default::default()
            },
        );

        // Markdown: enable only rust bridging
        let mut markdown_bridge = HashMap::new();
        markdown_bridge.insert(
            "rust".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                // highlights: None - Should inherit from wildcard
                bridge: Some(markdown_bridge),
                ..Default::default()
            },
        );

        // Test 1: "markdown" should have its own bridge filter (not wildcard's)
        let markdown =
            resolve_with_wildcard(&languages, "markdown", merge_language_settings).unwrap();
        assert!(
            markdown.queries.is_some(),
            "markdown should inherit queries from wildcard"
        );
        assert_eq!(
            markdown.queries.as_ref().unwrap()[0].path,
            "/default/highlights.scm",
            "markdown should inherit queries from wildcard"
        );
        // Bridge should be markdown-specific, not inherited from wildcard
        let bridge = markdown.bridge.as_ref().unwrap();
        assert!(
            bridge.get("rust").is_some(),
            "markdown bridge should have rust entry"
        );

        // Test 2: "quarto" (not defined) should get wildcard settings entirely
        let quarto = resolve_with_wildcard(&languages, "quarto", merge_language_settings).unwrap();
        assert!(
            quarto.queries.is_some(),
            "quarto should inherit queries from wildcard"
        );
        // Bridge should be wildcard's empty filter (disable all)
        let quarto_bridge = quarto.bridge.as_ref().unwrap();
        assert!(
            quarto_bridge.is_empty(),
            "quarto should inherit empty bridge filter from wildcard"
        );
    }

    /// PBI-155 Subtask 2: Test that LanguageSettings lookup uses wildcard resolution
    ///
    /// This test verifies the wiring: when we look up host language settings
    /// using WorkspaceSettings.languages (HashMap<String, LanguageSettings>),
    /// we should use wildcard resolution so that undefined languages inherit
    /// from languages._ settings.
    #[test]
    fn test_language_settings_wildcard_lookup_blocks_bridging_for_undefined_host() {
        let mut languages: HashMap<String, LanguageSettings> = HashMap::new();

        // Wildcard: block all bridging with empty filter
        languages.insert(
            "_".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::new()),
                ..Default::default()
            },
        );

        // Look up "quarto" which doesn't exist - should inherit from wildcard
        let quarto = resolve_with_wildcard(&languages, "quarto", merge_language_settings);
        assert!(
            quarto.is_some(),
            "Looking up undefined 'quarto' should return wildcard settings"
        );

        let quarto_settings = quarto.unwrap();
        // The wildcard has empty bridge filter, so is_language_bridgeable should return false
        assert!(
            !quarto_settings.is_language_bridgeable("rust"),
            "quarto (inherited from wildcard) should block bridging for rust"
        );
        assert!(
            !quarto_settings.is_language_bridgeable("python"),
            "quarto (inherited from wildcard) should block bridging for python"
        );
    }

    /// PBI-155 Subtask 3: Test that server lookup uses wildcard resolution
    ///
    /// This test verifies that when looking up a language server config by name,
    /// the wildcard server settings (languageServers._) are merged with specific
    /// server settings.
    ///
    /// Key behavior:
    /// - languageServers._ defines default initialization options
    /// - languageServers.rust-analyzer overrides only the cmd
    /// - The resolved rust-analyzer should have both cmd (from specific) and
    ///   initialization_options (inherited from wildcard)
    #[test]
    fn test_language_server_config_inherits_from_wildcard() {
        use serde_json::json;
        use settings::BridgeServerConfig;

        let mut servers: HashMap<String, BridgeServerConfig> = HashMap::new();

        // Wildcard server: default initialization options and workspace_type
        servers.insert(
            "_".to_string(),
            BridgeServerConfig {
                cmd: vec![],
                languages: vec![],
                initialization_options: Some(json!({ "checkOnSave": true })),
                workspace_type: Some(settings::WorkspaceType::Generic),
            },
        );

        // rust-analyzer: only specifies cmd and languages
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None, // Should inherit from wildcard
                workspace_type: None,         // Should inherit from wildcard
            },
        );

        // Test: rust-analyzer should merge with wildcard
        let ra =
            resolve_with_wildcard(&servers, "rust-analyzer", merge_bridge_server_configs).unwrap();

        // cmd from specific
        assert_eq!(ra.cmd, vec!["rust-analyzer".to_string()]);
        // languages from specific
        assert_eq!(ra.languages, vec!["rust".to_string()]);
        // initialization_options inherited from wildcard
        assert!(ra.initialization_options.is_some());
        let opts = ra.initialization_options.as_ref().unwrap();
        assert_eq!(opts.get("checkOnSave"), Some(&json!(true)));
        // workspace_type inherited from wildcard
        assert_eq!(ra.workspace_type, Some(settings::WorkspaceType::Generic));
    }

    /// Test that server lookup finds servers when languages list is inherited from wildcard.
    ///
    /// ADR-0011: When languageServers.rust-analyzer has empty languages but
    /// languageServers._ specifies languages = ["rust"], the lookup should still
    /// find rust-analyzer for Rust injections because the languages list is
    /// inherited from the wildcard during resolution.
    #[test]
    fn test_language_server_lookup_uses_resolved_languages_from_wildcard() {
        use settings::BridgeServerConfig;

        let mut servers: HashMap<String, BridgeServerConfig> = HashMap::new();

        // Wildcard server: specifies languages = ["rust", "python"]
        servers.insert(
            "_".to_string(),
            BridgeServerConfig {
                cmd: vec!["default-lsp".to_string()],
                languages: vec!["rust".to_string(), "python".to_string()],
                initialization_options: None,
                workspace_type: None,
            },
        );

        // rust-analyzer: specifies only cmd, inherits languages from wildcard
        servers.insert(
            "rust-analyzer".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec![], // Empty - should inherit from wildcard
                initialization_options: None,
                workspace_type: None,
            },
        );

        // Simulate the lookup logic from get_bridge_config_for_language:
        // For each server (excluding "_"), resolve it and check if it handles "rust"
        let injection_language = "rust";
        let mut found_server: Option<BridgeServerConfig> = None;

        for server_name in servers.keys() {
            if server_name == "_" {
                continue;
            }

            if let Some(resolved_config) =
                resolve_with_wildcard(&servers, server_name, merge_bridge_server_configs)
                && resolved_config
                    .languages
                    .iter()
                    .any(|l| l == injection_language)
            {
                found_server = Some(resolved_config);
                break;
            }
        }

        // Should find rust-analyzer because after resolution it has languages = ["rust", "python"]
        assert!(
            found_server.is_some(),
            "Should find a server for 'rust' when languages is inherited from wildcard"
        );
        let server = found_server.unwrap();
        assert_eq!(
            server.cmd,
            vec!["rust-analyzer".to_string()],
            "Should find rust-analyzer server"
        );
        assert!(
            server.languages.contains(&"rust".to_string()),
            "Resolved server should have 'rust' in languages (inherited from wildcard)"
        );
    }

    #[test]
    fn test_bridge_router_respects_host_filter() {
        // PBI-108 AC4: Bridge filtering is applied at request time before routing to language servers
        // This test verifies that is_language_bridgeable is correctly integrated into
        // the bridge routing logic.
        use settings::BridgeLanguageConfig;

        // Host markdown with bridge filter: only python and r enabled
        let mut bridge_filter = HashMap::new();
        bridge_filter.insert(
            "python".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        bridge_filter.insert(
            "r".to_string(),
            BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );
        let markdown_settings = LanguageSettings {
            bridge: Some(bridge_filter),
            ..Default::default()
        };

        // Router should allow python (enabled in filter)
        assert!(
            markdown_settings.is_language_bridgeable("python"),
            "Bridge router should allow python for markdown"
        );

        // Router should allow r (enabled in filter)
        assert!(
            markdown_settings.is_language_bridgeable("r"),
            "Bridge router should allow r for markdown"
        );

        // Router should block rust (not in filter)
        assert!(
            !markdown_settings.is_language_bridgeable("rust"),
            "Bridge router should block rust for markdown"
        );

        // Host quarto with no bridge filter (default: all)
        let quarto_settings = LanguageSettings::default();

        // Router should allow all languages
        assert!(
            quarto_settings.is_language_bridgeable("python"),
            "Bridge router should allow python for quarto (no filter)"
        );
        assert!(
            quarto_settings.is_language_bridgeable("rust"),
            "Bridge router should allow rust for quarto (no filter)"
        );

        // Host rmd with empty bridge filter (disable all)
        let rmd_settings = LanguageSettings {
            bridge: Some(HashMap::new()),
            ..Default::default()
        };

        // Router should block all languages
        assert!(
            !rmd_settings.is_language_bridgeable("r"),
            "Bridge router should block r for rmd (empty filter)"
        );
        assert!(
            !rmd_settings.is_language_bridgeable("python"),
            "Bridge router should block python for rmd (empty filter)"
        );
    }

    // Regression test: aliases must be merged in merge_languages()
    // Bug: When aliases field was added to LanguageSettings, merge_languages() was not
    // updated to merge it. This caused aliases from user config to be lost when
    // project config also defined the same language.

    #[test]
    fn test_merge_settings_languages_preserves_aliases() {
        // User config defines markdown with aliases=["rmd"]
        // Project config adds highlights for markdown but doesn't set aliases
        // Result should preserve aliases from user config
        let mut user_languages = HashMap::new();
        user_languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                parser: Some("/usr/lib/markdown.so".to_string()),
                aliases: Some(vec!["rmd".to_string(), "qmd".to_string()]),
                ..Default::default()
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: user_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        // Project only adds queries, doesn't set aliases
        let mut project_languages = HashMap::new();
        project_languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                queries: Some(vec![settings::QueryItem {
                    path: "./queries/markdown-highlights.scm".to_string(),
                    kind: Some(settings::QueryKind::Highlights),
                }]),
                // aliases: None - should inherit from user
                ..Default::default()
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: project_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        // Markdown should exist
        assert!(result.languages.contains_key("markdown"));
        let markdown = &result.languages["markdown"];

        // Parser: inherited from user
        assert_eq!(markdown.parser, Some("/usr/lib/markdown.so".to_string()));

        // Queries: overridden by project
        let queries = markdown.queries.as_ref().unwrap();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].path, "./queries/markdown-highlights.scm");

        // Aliases: MUST be inherited from user (this was the bug)
        assert!(
            markdown.aliases.is_some(),
            "aliases should be preserved from user config"
        );
        let aliases = markdown.aliases.as_ref().unwrap();
        assert_eq!(aliases.len(), 2);
        assert!(aliases.contains(&"rmd".to_string()));
        assert!(aliases.contains(&"qmd".to_string()));
    }

    #[test]
    fn test_merge_settings_languages_project_aliases_override_user() {
        // When project explicitly sets aliases, it should override user config
        let mut user_languages = HashMap::new();
        user_languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                aliases: Some(vec!["rmd".to_string()]),
                ..Default::default()
            },
        );

        let user_config = RawWorkspaceSettings {
            search_paths: None,
            languages: user_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        // Project overrides aliases
        let mut project_languages = HashMap::new();
        project_languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                aliases: Some(vec!["qmd".to_string(), "mdx".to_string()]),
                ..Default::default()
            },
        );

        let project_config = RawWorkspaceSettings {
            search_paths: None,
            languages: project_languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let result = merge_settings(Some(user_config), Some(project_config));
        assert!(result.is_some());
        let result = result.unwrap();

        let markdown = &result.languages["markdown"];

        // Aliases: project values should win
        assert!(markdown.aliases.is_some());
        let aliases = markdown.aliases.as_ref().unwrap();
        assert_eq!(aliases.len(), 2);
        assert!(aliases.contains(&"qmd".to_string()));
        assert!(aliases.contains(&"mdx".to_string()));
        assert!(!aliases.contains(&"rmd".to_string())); // User's alias should be gone
    }

    #[test]
    fn test_user_config_overrides_default_empty_string_mapping() {
        use crate::config::defaults::default_settings;

        // This tests the real-world scenario from issue investigation:
        // Default has markup.strong = "" (suppress)
        // User config has markup.strong = "keyword"
        // After merge, markup.strong should be "keyword"

        // User config from TOML (like ~/.config/kakehashi/kakehashi.toml)
        let user_config_content = r#"
            [captureMappings._.highlights]
            "markup.strong" = "keyword"
            "markup.heading.1" = "class"
        "#;

        let user_settings: RawWorkspaceSettings =
            toml::from_str(user_config_content).expect("should parse user config");

        // Get defaults (which have markup.strong = "")
        let defaults = default_settings();

        // Verify defaults have empty string for markup.strong
        assert_eq!(
            defaults.capture_mappings[WILDCARD_KEY].highlights["markup.strong"], "",
            "Defaults should suppress markup.strong with empty string"
        );

        // Merge: defaults < user (user overrides defaults)
        let merged = merge_settings(Some(defaults), Some(user_settings));
        assert!(merged.is_some());
        let merged = merged.unwrap();

        // After merge, user's "keyword" should override default's ""
        assert_eq!(
            merged.capture_mappings[WILDCARD_KEY].highlights["markup.strong"], "keyword",
            "User's markup.strong = 'keyword' should override default's ''"
        );

        // Also verify other user mappings are present
        assert_eq!(
            merged.capture_mappings[WILDCARD_KEY].highlights["markup.heading.1"], "class",
            "User's markup.heading.1 mapping should be present"
        );

        // Verify other defaults are still present
        assert_eq!(
            merged.capture_mappings[WILDCARD_KEY].highlights["variable.builtin"],
            "variable.defaultLibrary",
            "Default variable.builtin mapping should be inherited"
        );
    }

    #[test]
    fn test_resolve_with_wildcard_bridge_language_merges_fields() {
        // Wildcard provides aggregation defaults; specific provides enabled override
        let mut map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        map.insert(
            "_".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                aggregation: Some(HashMap::from([(
                    "_".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["server_a".to_string()],
                        ..Default::default()
                    },
                )])),
            },
        );
        map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(false),
                aggregation: None,
            },
        );

        let resolved =
            resolve_with_wildcard(&map, "python", merge_bridge_language_configs).unwrap();
        assert_eq!(
            resolved.enabled,
            Some(false),
            "specific enabled overrides wildcard"
        );
        assert!(
            resolved.aggregation.is_some(),
            "aggregation inherited from wildcard"
        );
        assert_eq!(
            resolved.aggregation.unwrap()["_"].priorities,
            vec!["server_a".to_string()]
        );
    }

    #[test]
    fn test_merge_bridge_maps_deep_merges_aggregation() {
        // Wildcard bridge has python with aggregation defaults
        // Specific bridge has python with enabled override but no aggregation
        // After merge, python should have both enabled override AND inherited aggregation
        let mut wildcard_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        wildcard_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                aggregation: Some(HashMap::from([(
                    "_".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["pyright".to_string()],
                        ..Default::default()
                    },
                )])),
            },
        );

        let mut specific_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        specific_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(false),
                aggregation: None, // no aggregation override
            },
        );

        let merged = merge_bridge_maps(&Some(wildcard_map), &Some(specific_map)).unwrap();
        let python = merged.get("python").unwrap();
        assert_eq!(
            python.enabled,
            Some(false),
            "specific enabled should override"
        );
        assert!(
            python.aggregation.is_some(),
            "aggregation should be inherited from wildcard"
        );
        assert_eq!(
            python.aggregation.as_ref().unwrap()["_"].priorities,
            vec!["pyright".to_string()]
        );
    }

    #[test]
    fn test_merge_bridge_maps_empty_overlay_clears_base() {
        // Empty overlay map means "disable all bridging" — should not inherit from base
        let mut base_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        base_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                ..Default::default()
            },
        );

        let empty_overlay: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();

        let merged = merge_bridge_maps(&Some(base_map), &Some(empty_overlay)).unwrap();
        assert!(merged.is_empty(), "empty overlay should clear base");
    }

    #[test]
    fn test_merge_bridge_maps_overlay_aggregation_wins_on_shared_keys() {
        // Both base and overlay define aggregation for the same method key.
        // Overlay should win for shared keys; base-only keys are preserved.
        let mut base_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        base_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                aggregation: Some(HashMap::from([
                    (
                        "_".to_string(),
                        settings::AggregationConfig {
                            priorities: vec!["base_default".to_string()],
                            ..Default::default()
                        },
                    ),
                    (
                        "textDocument/hover".to_string(),
                        settings::AggregationConfig {
                            priorities: vec!["base_hover".to_string()],
                            ..Default::default()
                        },
                    ),
                ])),
            },
        );

        let mut overlay_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        overlay_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["overlay_hover".to_string()],
                        ..Default::default()
                    },
                )])),
            },
        );

        let merged = merge_bridge_maps(&Some(base_map), &Some(overlay_map)).unwrap();
        let python = merged.get("python").unwrap();

        // enabled: overlay has None, so base's Some(true) wins
        assert_eq!(python.enabled, Some(true));

        let agg = python.aggregation.as_ref().unwrap();

        // Shared key "textDocument/hover": overlay wins
        assert_eq!(
            agg["textDocument/hover"].priorities,
            vec!["overlay_hover".to_string()],
            "overlay should win for shared aggregation keys"
        );

        // Base-only key "_": preserved from base
        assert_eq!(
            agg["_"].priorities,
            vec!["base_default".to_string()],
            "base-only aggregation keys should be preserved"
        );
    }

    #[test]
    fn test_merge_bridge_maps_overlay_aggregation_preserves_strategy_when_atomic() {
        // When overlay replaces an aggregation entry, the entire entry
        // (including strategy) is replaced atomically — base's strategy
        // does NOT leak into overlay's entry.
        let mut base_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        base_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                aggregation: Some(HashMap::from([(
                    "textDocument/diagnostic".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["ruff".to_string()],
                        strategy: Some(settings::AggregationStrategy::Concatenated),
                        ..Default::default()
                    },
                )])),
            },
        );

        let mut overlay_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        overlay_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(HashMap::from([(
                    "textDocument/diagnostic".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["pyright".to_string()],
                        strategy: Some(settings::AggregationStrategy::Preferred),
                        ..Default::default()
                    },
                )])),
            },
        );

        let merged = merge_bridge_maps(&Some(base_map), &Some(overlay_map)).unwrap();
        let python = merged.get("python").unwrap();
        let agg = python.aggregation.as_ref().unwrap();
        let diag = &agg["textDocument/diagnostic"];

        // Overlay wins entirely for the shared key
        assert_eq!(
            diag.priorities,
            vec!["pyright".to_string()],
            "overlay priorities should win"
        );
        assert_eq!(
            diag.strategy,
            Some(settings::AggregationStrategy::Preferred),
            "overlay strategy should win atomically"
        );
    }

    #[test]
    fn test_merge_bridge_maps_preserves_max_fan_out_from_base_only_keys() {
        // Overlay omits method key entirely → base maxFanOut preserved
        let mut base_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        base_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                aggregation: Some(HashMap::from([(
                    "_".to_string(),
                    settings::AggregationConfig {
                        max_fan_out: Some(3),
                        ..Default::default()
                    },
                )])),
            },
        );

        let mut overlay_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        overlay_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["pyright".to_string()],
                        ..Default::default()
                    },
                )])),
            },
        );

        let merged = merge_bridge_maps(&Some(base_map), &Some(overlay_map)).unwrap();
        let python = merged.get("python").unwrap();
        let agg = python.aggregation.as_ref().unwrap();

        // Base-only key "_" is preserved (overlay didn't touch it)
        assert_eq!(
            agg["_"].max_fan_out,
            Some(3),
            "base maxFanOut should be preserved when overlay omits the method key"
        );
    }

    #[test]
    fn test_merge_bridge_maps_overlay_same_key_without_max_fan_out_replaces_atomically() {
        // Overlay includes same method key without maxFanOut →
        // base maxFanOut is lost (atomic per-key replacement)
        let mut base_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        base_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: Some(true),
                aggregation: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["ruff".to_string()],
                        max_fan_out: Some(2),
                        ..Default::default()
                    },
                )])),
            },
        );

        let mut overlay_map: HashMap<String, settings::BridgeLanguageConfig> = HashMap::new();
        overlay_map.insert(
            "python".to_string(),
            settings::BridgeLanguageConfig {
                enabled: None,
                aggregation: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    settings::AggregationConfig {
                        priorities: vec!["pyright".to_string()],
                        ..Default::default()
                    },
                )])),
            },
        );

        let merged = merge_bridge_maps(&Some(base_map), &Some(overlay_map)).unwrap();
        let python = merged.get("python").unwrap();
        let agg = python.aggregation.as_ref().unwrap();
        let hover = &agg["textDocument/hover"];

        assert_eq!(
            hover.priorities,
            vec!["pyright".to_string()],
            "overlay priorities should win"
        );
        assert_eq!(
            hover.max_fan_out, None,
            "base maxFanOut should be lost when overlay replaces the same method key atomically"
        );
    }
}

#[cfg(test)]
mod try_from_settings_tests {
    use super::*;
    use expand::make_env;
    use settings::{QueryItem, QueryKind};
    use std::collections::HashMap;

    #[test]
    fn expands_search_paths() {
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["$TEST_VAR/parsers".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let env = make_env(&[("TEST_VAR", "/home/user")]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();
        assert_eq!(ws.search_paths, vec!["/home/user/parsers"]);
    }

    #[test]
    fn expands_parser_path() {
        let mut languages = HashMap::new();
        languages.insert(
            "lua".to_string(),
            LanguageSettings {
                parser: Some("$TEST_VAR/lua.so".to_string()),
                queries: None,
                bridge: None,
                aliases: None,
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let env = make_env(&[("TEST_VAR", "/opt/parsers")]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();
        assert_eq!(
            ws.languages.get("lua").unwrap().parser.as_deref(),
            Some("/opt/parsers/lua.so")
        );
    }

    #[test]
    fn expands_query_path() {
        let mut languages = HashMap::new();
        languages.insert(
            "lua".to_string(),
            LanguageSettings {
                parser: None,
                queries: Some(vec![QueryItem {
                    path: "${TEST_VAR}/highlights.scm".to_string(),
                    kind: Some(QueryKind::Highlights),
                }]),
                bridge: None,
                aliases: None,
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let env = make_env(&[("TEST_VAR", "/queries")]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();
        let queries = ws.languages.get("lua").unwrap().queries.as_ref().unwrap();
        assert_eq!(queries[0].path, "/queries/highlights.scm");
    }

    #[test]
    fn undefined_var_returns_error() {
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["$UNDEFINED/path".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let env = make_env(&[]);
        let errs = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap_err();
        assert_eq!(
            errs.0,
            vec![expand::ExpandError::UndefinedVar {
                var_name: "UNDEFINED".to_string(),
                input: "$UNDEFINED/path".to_string(),
            }]
        );
    }

    #[test]
    fn collects_all_expansion_errors() {
        let mut languages = HashMap::new();
        languages.insert(
            "lua".to_string(),
            LanguageSettings {
                parser: Some("$ALSO_MISSING/lua.so".to_string()),
                queries: None,
                bridge: None,
                aliases: None,
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["$MISSING_ONE/parsers".to_string()]),
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let env = make_env(&[]);
        let errs = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap_err();
        assert_eq!(
            errs.0.len(),
            2,
            "Should collect errors from all path fields"
        );
    }

    #[test]
    fn tilde_without_home_dir_returns_error() {
        let settings = RawWorkspaceSettings {
            search_paths: Some(vec!["~/parsers".to_string()]),
            languages: HashMap::new(),
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };
        let env = make_env(&[]);
        let errs = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap_err();
        assert_eq!(
            errs.0,
            vec![expand::ExpandError::NoHomeDir {
                input: "~/parsers".to_string(),
            }]
        );
    }
}
