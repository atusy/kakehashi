pub mod defaults;
pub(crate) mod expand;
pub(crate) mod merge;
pub mod settings;

#[cfg(test)]
pub(crate) use expand::make_env;
pub(crate) mod user;

pub use expand::{set_config_file_override, set_data_dir_override};
pub(crate) use merge::{
    merge_aggregation_configs, merge_bridge_language_configs, merge_bridge_server_configs,
    merge_language_settings, merge_workspace_settings, resolve_with_wildcard,
};
pub(crate) use settings::{CaptureMappings, QueryTypeMappings};
pub use settings::{LanguageSettings, RawWorkspaceSettings, WorkspaceSettings, json_schema};
pub(crate) use user::load_user_config;

/// Wildcard key for default configurations in HashMap-based settings.
/// Used in capture_mappings, languages, and language_servers for fallback values.
pub(crate) const WILDCARD_KEY: &str = "_";

/// Returns the default search paths for parsers and queries.
/// Uses the platform-specific data directory (via `dirs` crate):
/// - Linux: ~/.local/share/kakehashi
/// - macOS: ~/Library/Application Support/kakehashi
/// - Windows: %APPDATA%/kakehashi
///
/// Note: Returns the base directory only. The resolver functions append
/// "parser/" or "queries/" subdirectories as needed.
fn default_search_paths() -> Vec<String> {
    crate::install::default_data_dir()
        .map(|d| vec![d.to_string_lossy().to_string()])
        .unwrap_or_default()
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

        // Resolve base configs first so expansion only sees effective parser/query paths.
        ws.languages = merge::resolve_base_configs(&ws.languages);

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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::collections::HashMap;

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
                ..Default::default()
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
                queries: Some(vec![QueryItem {
                    path: "${TEST_VAR}/highlights.scm".to_string(),
                    kind: Some(QueryKind::Highlights),
                }]),
                ..Default::default()
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
    fn resolves_base_before_expanding_derived_paths() {
        // With most-specific-wins, derived parser is kept and expanded.
        // If derived has no parser, it inherits base's parser path,
        // which must be expandable.
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                parser: Some("/opt/parsers/markdown.so".to_string()),
                ..Default::default()
            },
        );
        languages.insert(
            "rmd".to_string(),
            LanguageSettings {
                base: Some("markdown".to_string()),
                // No parser → inherits markdown's parser
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            search_paths: None,
            languages,
            capture_mappings: HashMap::new(),
            auto_install: None,
            language_servers: None,
        };

        let env = make_env(&[]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env)
            .expect("inherited parser path should be expanded successfully");

        assert_eq!(
            ws.languages.get("rmd").unwrap().parser.as_deref(),
            Some("/opt/parsers/markdown.so")
        );
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
                ..Default::default()
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

    #[test]
    fn base_config_most_specific_wins() {
        let mut languages = HashMap::new();
        languages.insert(
            "markdown".to_string(),
            LanguageSettings {
                parser: Some("/opt/markdown.so".to_string()),
                queries: Some(vec![crate::config::settings::QueryItem {
                    path: "/opt/markdown/highlights.scm".to_string(),
                    kind: Some(crate::config::settings::QueryKind::Highlights),
                }]),
                ..Default::default()
            },
        );
        languages.insert(
            "rmd".to_string(),
            LanguageSettings {
                base: Some("markdown".to_string()),
                parser: Some("/opt/rmd.so".to_string()),
                ..Default::default()
            },
        );
        let settings = RawWorkspaceSettings {
            languages,
            ..Default::default()
        };
        let env = make_env(&[]);
        let ws = WorkspaceSettings::try_from_settings(&settings, None, env).unwrap();

        // rmd's own parser wins (most-specific-wins)
        assert_eq!(ws.languages["rmd"].parser.as_deref(), Some("/opt/rmd.so"));
        // queries inherited from markdown (rmd didn't set them)
        assert!(ws.languages["rmd"].queries.is_some());
        // base field should be preserved
        assert_eq!(ws.languages["rmd"].base, Some("markdown".to_string()));
    }
}
