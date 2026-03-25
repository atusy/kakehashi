use super::settings::{BridgeLanguageConfig, BridgeServerConfig, LanguageSettings};
use super::{CaptureMappings, RawWorkspaceSettings, WILDCARD_KEY};
use std::collections::HashMap;

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
    base: &BridgeLanguageConfig,
    overlay: &BridgeLanguageConfig,
) -> BridgeLanguageConfig {
    BridgeLanguageConfig {
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
    base: &BridgeServerConfig,
    overlay: &BridgeServerConfig,
) -> BridgeServerConfig {
    BridgeServerConfig {
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
    }
}

/// Field-level merge of two LanguageSettings values.
/// Option fields: overlay wins when present; base provides defaults.
/// Bridge HashMaps: deep merged via merge_bridge_maps.
pub(crate) fn merge_language_settings(
    base: &LanguageSettings,
    overlay: &LanguageSettings,
) -> LanguageSettings {
    LanguageSettings {
        base: overlay.base.clone().or_else(|| base.base.clone()),
        parser: overlay.parser.clone().or_else(|| base.parser.clone()),
        queries: overlay.queries.clone().or_else(|| base.queries.clone()),
        bridge: merge_bridge_maps(base.bridge.as_ref(), overlay.bridge.as_ref()),
        aliases: overlay.aliases.clone().or_else(|| base.aliases.clone()),
    }
}

/// Resolve base configs: for each language with `base = Some(name)`,
/// replace derived language's parser/queries/bridge with the base's raw config.
///
/// Single-level only: if the base itself has a `base`, a warning is logged
/// but chain walking is not performed.
pub(crate) fn resolve_base_configs(
    languages: &HashMap<String, LanguageSettings>,
) -> HashMap<String, LanguageSettings> {
    languages
        .iter()
        .map(|(name, settings)| {
            let resolved = match settings.base.as_deref() {
                // No base → keep as-is
                None => settings.clone(),
                // Self-reference → keep as-is (coordinator surfaces the warning)
                Some(base_name) if base_name == name => settings.clone(),
                // Normal base → inherit parser/queries/bridge from base
                Some(base_name) => {
                    let base_config = languages.get(base_name).cloned().unwrap_or_default();

                    if base_config.base.is_some() {
                        log::warn!(
                            target: "kakehashi::config",
                            "Language '{}' has base='{}', which itself has a base. \
                             Multi-level base chains are not yet supported; \
                             '{}' will inherit '{}' config as-is.",
                            name, base_name, name, base_name
                        );
                    }

                    LanguageSettings {
                        parser: base_config.parser,
                        queries: base_config.queries,
                        bridge: base_config.bridge,
                        ..settings.clone()
                    }
                }
            };
            (name.clone(), resolved)
        })
        .collect()
}

/// Deep merge two optional bridge HashMaps.
///
/// When both base and overlay exist:
/// - If overlay is empty (`Some({})`), it completely replaces base (empty-means-clear).
///   This preserves the "empty map = disable all bridging" contract.
/// - Otherwise, per-key entries are merged at the field level using
///   [`merge_bridge_language_configs`].
fn merge_bridge_maps(
    base: Option<&HashMap<String, BridgeLanguageConfig>>,
    overlay: Option<&HashMap<String, BridgeLanguageConfig>>,
) -> Option<HashMap<String, BridgeLanguageConfig>> {
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

/// Merge two RawWorkspaceSettings, preferring values from `overlay` over `base`
pub(crate) fn merge_workspace_settings(
    base: Option<RawWorkspaceSettings>,
    overlay: Option<RawWorkspaceSettings>,
) -> Option<RawWorkspaceSettings> {
    match (base, overlay) {
        (Some(base), Some(overlay)) => {
            let merged = RawWorkspaceSettings {
                search_paths: overlay.search_paths.or(base.search_paths),
                languages: merge_languages(base.languages, overlay.languages),
                capture_mappings: merge_capture_mappings(
                    base.capture_mappings,
                    overlay.capture_mappings,
                ),
                auto_install: overlay.auto_install.or(base.auto_install),
                language_servers: merge_language_servers(
                    base.language_servers,
                    overlay.language_servers,
                ),
            };
            Some(merged)
        }
        (base, overlay) => base.or(overlay),
    }
}

fn merge_languages(
    mut base: HashMap<String, LanguageSettings>,
    overlay: HashMap<String, LanguageSettings>,
) -> HashMap<String, LanguageSettings> {
    for (key, overlay_config) in overlay {
        base.entry(key)
            .and_modify(|base_config| {
                *base_config = merge_language_settings(base_config, &overlay_config);
            })
            .or_insert(overlay_config);
    }
    base
}

fn merge_language_servers(
    base: Option<HashMap<String, BridgeServerConfig>>,
    overlay: Option<HashMap<String, BridgeServerConfig>>,
) -> Option<HashMap<String, BridgeServerConfig>> {
    match (base, overlay) {
        (None, None) => None,
        (Some(servers), None) | (None, Some(servers)) => Some(servers),
        (Some(_), Some(overlay_servers)) if overlay_servers.is_empty() => {
            Some(HashMap::new()) // empty-means-clear
        }
        (Some(mut base_servers), Some(overlay_servers)) => {
            // Deep merge: overlay values override base values for the same key
            for (key, overlay_config) in overlay_servers {
                base_servers
                    .entry(key)
                    .and_modify(|base_config| {
                        *base_config = merge_bridge_server_configs(base_config, &overlay_config);
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
        let base_mappings = base.entry(lang).or_default();
        base_mappings.highlights.extend(overlay_mappings.highlights);
        base_mappings.locals.extend(overlay_mappings.locals);
        base_mappings.folds.extend(overlay_mappings.folds);
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QueryTypeMappings;
    use crate::config::settings;
    use std::collections::HashMap;

    // ========================================================================
    // merge_workspace_settings: Option combinator tests
    // ========================================================================

    #[test]
    fn test_merge_workspace_settings_none_combinations() {
        // None + None = None
        assert!(merge_workspace_settings(None, None).is_none());

        // Some + None = Some (base returned)
        let base = RawWorkspaceSettings {
            search_paths: Some(vec!["/base".to_string()]),
            ..Default::default()
        };
        let result = merge_workspace_settings(Some(base), None).unwrap();
        assert_eq!(result.search_paths, Some(vec!["/base".to_string()]));

        // None + Some = Some (overlay returned)
        let overlay = RawWorkspaceSettings {
            search_paths: Some(vec!["/overlay".to_string()]),
            ..Default::default()
        };
        let result = merge_workspace_settings(None, Some(overlay)).unwrap();
        assert_eq!(result.search_paths, Some(vec!["/overlay".to_string()]));
    }

    // ========================================================================
    // merge_workspace_settings: overlay-wins and deep-merge across all fields
    // ========================================================================

    /// Exercises all five RawWorkspaceSettings fields in a single merge call.
    ///
    /// Tests three merge behaviors simultaneously:
    /// - Scalar/Option fields: overlay wins (search_paths, auto_install)
    /// - HashMap fields with shared keys: deep-merged (languages, language_servers,
    ///   capture_mappings) — overlay values override per-key, base-only keys preserved
    /// - HashMap fields with disjoint keys: union of both sides
    #[test]
    fn test_merge_workspace_settings_all_fields() {
        use serde_json::json;
        use settings::{BridgeLanguageConfig, BridgeServerConfig};

        // ── base ──────────────────────────────────────────────────────
        let base = RawWorkspaceSettings {
            search_paths: Some(vec!["/base/path".to_string()]),
            auto_install: Some(true),
            languages: HashMap::from([
                (
                    "python".to_string(),
                    LanguageSettings {
                        parser: Some("/base/python.so".to_string()),
                        queries: Some(vec![settings::QueryItem {
                            path: "/base/python-highlights.scm".to_string(),
                            kind: Some(settings::QueryKind::Highlights),
                        }]),
                        bridge: Some(HashMap::from([(
                            "rust".to_string(),
                            BridgeLanguageConfig {
                                enabled: Some(true),
                                ..Default::default()
                            },
                        )])),
                        aliases: Some(vec!["py3".to_string()]),
                        ..Default::default()
                    },
                ),
                (
                    "lua".to_string(),
                    LanguageSettings {
                        parser: Some("/base/lua.so".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            language_servers: Some(HashMap::from([
                (
                    "rust-analyzer".to_string(),
                    BridgeServerConfig {
                        cmd: vec!["rust-analyzer".to_string()],
                        languages: vec!["rust".to_string()],
                        initialization_options: Some(json!({"checkOnSave": true})),
                    },
                ),
                (
                    "lua-language-server".to_string(),
                    BridgeServerConfig {
                        cmd: vec!["lua-language-server".to_string()],
                        languages: vec!["lua".to_string()],
                        initialization_options: None,
                    },
                ),
            ])),
            capture_mappings: HashMap::from([
                (
                    "_".to_string(),
                    QueryTypeMappings {
                        highlights: HashMap::from([
                            ("variable.builtin".to_string(), "base.variable".to_string()),
                            ("function.builtin".to_string(), "base.function".to_string()),
                        ]),
                        locals: HashMap::from([(
                            "definition.var".to_string(),
                            "base.definition".to_string(),
                        )]),
                        folds: HashMap::from([(
                            "fold.comment".to_string(),
                            "base.comment".to_string(),
                        )]),
                    },
                ),
                (
                    "lua".to_string(),
                    QueryTypeMappings {
                        highlights: HashMap::from([(
                            "keyword".to_string(),
                            "base.keyword".to_string(),
                        )]),
                        ..Default::default()
                    },
                ),
            ]),
        };

        // ── overlay ───────────────────────────────────────────────────
        let overlay = RawWorkspaceSettings {
            search_paths: Some(vec!["/overlay/path".to_string()]),
            auto_install: Some(false),
            languages: HashMap::from([
                (
                    // shared key: python — overlay overrides queries, inherits parser/bridge/aliases
                    "python".to_string(),
                    LanguageSettings {
                        queries: Some(vec![settings::QueryItem {
                            path: "./overlay/python-highlights.scm".to_string(),
                            kind: Some(settings::QueryKind::Highlights),
                        }]),
                        ..Default::default()
                    },
                ),
                (
                    // new key: rust — added by overlay
                    "rust".to_string(),
                    LanguageSettings {
                        parser: Some("/overlay/rust.so".to_string()),
                        ..Default::default()
                    },
                ),
            ]),
            language_servers: Some(HashMap::from([
                (
                    // shared key: rust-analyzer — overlay adds initOptions, inherits cmd/languages
                    "rust-analyzer".to_string(),
                    BridgeServerConfig {
                        cmd: vec![],
                        languages: vec![],
                        initialization_options: Some(json!({"linkedProjects": ["./Cargo.toml"]})),
                    },
                ),
                (
                    // new key: pyright — added by overlay
                    "pyright".to_string(),
                    BridgeServerConfig {
                        cmd: vec!["pyright-langserver".to_string(), "--stdio".to_string()],
                        languages: vec!["python".to_string()],
                        initialization_options: None,
                    },
                ),
            ])),
            capture_mappings: HashMap::from([
                (
                    // shared key: _ — overlay overrides variable.builtin, adds type.builtin;
                    //   overrides locals definition.var; adds folds fold.function
                    "_".to_string(),
                    QueryTypeMappings {
                        highlights: HashMap::from([
                            (
                                "variable.builtin".to_string(),
                                "overlay.variable".to_string(),
                            ),
                            ("type.builtin".to_string(), "overlay.type".to_string()),
                        ]),
                        locals: HashMap::from([(
                            "definition.var".to_string(),
                            "overlay.definition".to_string(),
                        )]),
                        folds: HashMap::from([(
                            "fold.function".to_string(),
                            "overlay.function".to_string(),
                        )]),
                    },
                ),
                (
                    // new key: rust — added by overlay
                    "rust".to_string(),
                    QueryTypeMappings {
                        highlights: HashMap::from([(
                            "type.builtin".to_string(),
                            "rust.type".to_string(),
                        )]),
                        ..Default::default()
                    },
                ),
            ]),
        };

        // ── merge & snapshot ──────────────────────────────────────────
        let result = merge_workspace_settings(Some(base), Some(overlay)).unwrap();
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(result);
        });
    }

    // ========================================================================
    // merge_workspace_settings: empty-means-clear
    // ========================================================================

    #[test]
    fn test_merge_workspace_settings_empty_language_servers_overlay_clears_base() {
        use settings::BridgeServerConfig;

        let base = RawWorkspaceSettings {
            language_servers: Some(HashMap::from([(
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec!["rust".to_string()],
                    initialization_options: None,
                },
            )])),
            ..Default::default()
        };
        let overlay = RawWorkspaceSettings {
            language_servers: Some(HashMap::new()),
            ..Default::default()
        };

        let result = merge_workspace_settings(Some(base), Some(overlay)).unwrap();
        assert!(
            result.language_servers.unwrap().is_empty(),
            "empty overlay should clear base language servers"
        );
    }

    // ========================================================================
    // merge_workspace_settings: bridge sub-map deep merge
    // ========================================================================

    #[test]
    fn test_merge_workspace_settings_languages_bridge_deep_merge() {
        // When both base and overlay define bridge maps for the same language,
        // bridge entries should be deep-merged per-key (not shallow-replaced).
        // Base has rust bridge; overlay adds javascript bridge.
        // Result should contain both rust and javascript.
        use settings::BridgeLanguageConfig;

        let base_config = RawWorkspaceSettings {
            languages: HashMap::from([(
                "python".to_string(),
                LanguageSettings {
                    bridge: Some(HashMap::from([(
                        "rust".to_string(),
                        BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let overlay_config = RawWorkspaceSettings {
            languages: HashMap::from([(
                "python".to_string(),
                LanguageSettings {
                    bridge: Some(HashMap::from([(
                        "javascript".to_string(),
                        BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        let result = merge_workspace_settings(Some(base_config), Some(overlay_config)).unwrap();
        let bridge = result.languages["python"]
            .bridge
            .as_ref()
            .expect("bridge should be Some");

        // Both keys should be present (deep merge, not replacement)
        assert!(bridge.contains_key("rust"));
        assert!(bridge.contains_key("javascript"));
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
        let languages = HashMap::from([
            // Wildcard language with wildcard bridge (default enabled = true)
            (
                "_".to_string(),
                LanguageSettings {
                    parser: Some("/default/path.so".to_string()),
                    queries: Some(vec![settings::QueryItem {
                        path: "/default/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    }]),
                    bridge: Some(HashMap::from([(
                        "_".to_string(),
                        settings::BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
            // Python-specific: disable bridging to JavaScript, but inherit _ for parser
            (
                "python".to_string(),
                LanguageSettings {
                    // parser: None - Should inherit from _
                    // queries: None - Should inherit from _
                    bridge: Some(HashMap::from([(
                        "javascript".to_string(),
                        settings::BridgeLanguageConfig {
                            enabled: Some(false),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
        ]);

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
        assert_eq!(
            js_resolved.unwrap().enabled,
            Some(false),
            "Python's javascript bridge should be disabled (override)"
        );

        // Rust: inherited from _.bridge._ through deep merge
        // ADR-0011: bridge maps are deep merged, so python gets wildcard's bridge._
        let rust_resolved = resolve_with_wildcard(bridge, "rust", merge_bridge_language_configs);
        assert!(
            rust_resolved.is_some(),
            "Python's rust bridge should resolve (inherited from wildcard's bridge._)"
        );
        assert_eq!(
            rust_resolved.unwrap().enabled,
            Some(true),
            "Python's rust bridge should be enabled (from wildcard's bridge._)"
        );
    }

    #[test]
    fn test_specific_bridge_with_nested_wildcard() {
        // ADR-0011: Test case where python.bridge includes _ wildcard
        // - languages.python.bridge._ = enabled: true (python-specific default)
        // - languages.python.bridge.javascript = enabled: false (override)
        // - rust should inherit from python.bridge._ (enabled = true)
        // Python with its own wildcard bridge
        let languages = HashMap::from([(
            "python".to_string(),
            LanguageSettings {
                parser: Some("/python/path.so".to_string()),
                bridge: Some(HashMap::from([
                    (
                        "_".to_string(),
                        settings::BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        }, // Python's own default
                    ),
                    (
                        "javascript".to_string(),
                        settings::BridgeLanguageConfig {
                            enabled: Some(false),
                            ..Default::default()
                        }, // Override for JS
                    ),
                ])),
                ..Default::default()
            },
        )]);

        let resolved_lang = resolve_with_wildcard(&languages, "python", merge_language_settings);
        assert!(resolved_lang.is_some());
        let lang_config = resolved_lang.unwrap();
        let bridge = lang_config.bridge.as_ref().unwrap();

        // JavaScript: specific override
        let js_resolved =
            resolve_with_wildcard(bridge, "javascript", merge_bridge_language_configs);
        assert!(js_resolved.is_some());
        assert_eq!(
            js_resolved.unwrap().enabled,
            Some(false),
            "JavaScript should be disabled"
        );

        // Rust: inherits from python's bridge._
        let rust_resolved = resolve_with_wildcard(bridge, "rust", merge_bridge_language_configs);
        assert!(rust_resolved.is_some());
        assert_eq!(
            rust_resolved.unwrap().enabled,
            Some(true),
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
        // Wildcard language with wildcard bridge
        let languages = HashMap::from([(
            "_".to_string(),
            LanguageSettings {
                parser: Some("/default/path.so".to_string()),
                bridge: Some(HashMap::from([(
                    "_".to_string(),
                    settings::BridgeLanguageConfig {
                        enabled: Some(true),
                        ..Default::default()
                    },
                )])),
                ..Default::default()
            },
        )]);

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
        assert_eq!(
            resolved_bridge.unwrap().enabled,
            Some(true),
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
        let languages = HashMap::from([
            // Wildcard has default bridge settings for rust and go
            (
                "_".to_string(),
                LanguageSettings {
                    parser: Some("/default/path.so".to_string()),
                    bridge: Some(HashMap::from([
                        (
                            "rust".to_string(),
                            settings::BridgeLanguageConfig {
                                enabled: Some(true),
                                ..Default::default()
                            },
                        ),
                        (
                            "go".to_string(),
                            settings::BridgeLanguageConfig {
                                enabled: Some(true),
                                ..Default::default()
                            },
                        ),
                    ])),
                    ..Default::default()
                },
            ),
            // Python adds javascript bridge but should inherit rust/go from wildcard
            (
                "python".to_string(),
                LanguageSettings {
                    // parser: None - Inherits from wildcard
                    bridge: Some(HashMap::from([(
                        "javascript".to_string(),
                        settings::BridgeLanguageConfig {
                            enabled: Some(false),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
        ]);

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

    /// ADR-0011: resolve_with_wildcard covers all 4 match arms for language servers.
    ///
    /// - Neither wildcard nor specific → None
    /// - Wildcard only → cloned wildcard
    /// - Specific only → cloned specific
    /// - Both → merged (specific overrides wildcard, empty Vec inherits)
    #[test]
    fn test_resolve_language_server_wildcard_combinations() {
        use serde_json::json;
        use settings::BridgeServerConfig;

        // Neither → None
        let empty: HashMap<String, BridgeServerConfig> = HashMap::new();
        assert!(resolve_with_wildcard(&empty, "ra", merge_bridge_server_configs).is_none());

        // Wildcard only → cloned
        let servers = HashMap::from([(
            "_".to_string(),
            BridgeServerConfig {
                cmd: vec!["default-lsp".to_string()],
                languages: vec!["any".to_string()],
                initialization_options: None,
            },
        )]);
        let resolved = resolve_with_wildcard(&servers, "ra", merge_bridge_server_configs).unwrap();
        assert_eq!(resolved.cmd, vec!["default-lsp".to_string()]);
        assert_eq!(resolved.languages, vec!["any".to_string()]);

        // Specific only → cloned
        let servers = HashMap::from([(
            "ra".to_string(),
            BridgeServerConfig {
                cmd: vec!["rust-analyzer".to_string()],
                languages: vec!["rust".to_string()],
                initialization_options: None,
            },
        )]);
        let resolved = resolve_with_wildcard(&servers, "ra", merge_bridge_server_configs).unwrap();
        assert_eq!(resolved.cmd, vec!["rust-analyzer".to_string()]);
        assert_eq!(resolved.languages, vec!["rust".to_string()]);

        // Both → merged (specific cmd wins, empty languages inherits, initOptions deep-merged)
        let servers = HashMap::from([
            (
                "_".to_string(),
                BridgeServerConfig {
                    cmd: vec!["default-lsp".to_string()],
                    languages: vec!["any".to_string()],
                    initialization_options: Some(json!({"defaultOption": true})),
                },
            ),
            (
                "ra".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec![],
                    initialization_options: Some(json!({"linkedProjects": ["./Cargo.toml"]})),
                },
            ),
        ]);
        let resolved = resolve_with_wildcard(&servers, "ra", merge_bridge_server_configs).unwrap();
        assert_eq!(resolved.cmd, vec!["rust-analyzer".to_string()]);
        assert_eq!(resolved.languages, vec!["any".to_string()]);
        let init_opts = resolved.initialization_options.unwrap();
        assert_eq!(init_opts.get("defaultOption"), Some(&json!(true)));
        assert_eq!(
            init_opts.get("linkedProjects"),
            Some(&json!(["./Cargo.toml"]))
        );
    }

    // PBI-157: Deep merge for initialization_options (ADR-0010)

    /// ADR-0010: initialization_options deep merge covers three behaviors:
    /// - Disjoint keys: both preserved (feature1 from base, feature2 from overlay)
    /// - Same key: overlay wins (shared_opt = "overlay")
    /// - Nested objects: recursively merged (nested.base_only + nested.overlay_only)
    #[test]
    fn test_merge_bridge_server_configs_initialization_options_deep_merge() {
        use serde_json::json;
        use settings::BridgeServerConfig;

        let base = BridgeServerConfig {
            cmd: vec!["default-lsp".to_string()],
            languages: vec![],
            initialization_options: Some(json!({
                "feature1": true,
                "shared_opt": "base",
                "nested": { "base_only": 1, "shared": "base" }
            })),
        };
        let overlay = BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec!["rust".to_string()],
            initialization_options: Some(json!({
                "feature2": true,
                "shared_opt": "overlay",
                "nested": { "overlay_only": 2, "shared": "overlay" }
            })),
        };

        let resolved = merge_bridge_server_configs(&base, &overlay);
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(resolved.initialization_options);
        });
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

        let languages = HashMap::from([
            // Wildcard language: disable bridging by default (empty bridge filter)
            (
                "_".to_string(),
                LanguageSettings {
                    queries: Some(vec![settings::QueryItem {
                        path: "/default/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    }]),
                    bridge: Some(HashMap::new()), // Empty = disable all bridging
                    ..Default::default()
                },
            ),
            // Markdown: enable only rust bridging
            (
                "markdown".to_string(),
                LanguageSettings {
                    // highlights: None - Should inherit from wildcard
                    bridge: Some(HashMap::from([(
                        "rust".to_string(),
                        BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
        ]);

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
        // Wildcard: block all bridging with empty filter
        let languages = HashMap::from([(
            "_".to_string(),
            LanguageSettings {
                bridge: Some(HashMap::new()),
                ..Default::default()
            },
        )]);

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

        let servers = HashMap::from([
            // Wildcard server: default initialization options
            (
                "_".to_string(),
                BridgeServerConfig {
                    cmd: vec![],
                    languages: vec![],
                    initialization_options: Some(json!({ "checkOnSave": true })),
                },
            ),
            // rust-analyzer: only specifies cmd and languages
            (
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec!["rust".to_string()],
                    initialization_options: None, // Should inherit from wildcard
                },
            ),
        ]);

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

        let servers = HashMap::from([
            // Wildcard server: specifies languages = ["rust", "python"]
            (
                "_".to_string(),
                BridgeServerConfig {
                    cmd: vec!["default-lsp".to_string()],
                    languages: vec!["rust".to_string(), "python".to_string()],
                    initialization_options: None,
                },
            ),
            // rust-analyzer: specifies only cmd, inherits languages from wildcard
            (
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec![], // Empty - should inherit from wildcard
                    initialization_options: None,
                },
            ),
        ]);

        // Simulate the lookup logic from get_bridge_config_for_language:
        // For each server (excluding "_"), resolve it and check if it handles "rust"
        let injection_language = "rust";
        let mut found_server = None;

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

    // Alias inheritance (overlay=None inherits from base) is covered by
    // test_merge_workspace_settings_all_fields snapshot (python.aliases = ["py3"]).

    #[test]
    fn test_merge_workspace_settings_languages_project_aliases_override_user() {
        // When project explicitly sets aliases, it should override user config
        let user_config = RawWorkspaceSettings {
            languages: HashMap::from([(
                "markdown".to_string(),
                LanguageSettings {
                    aliases: Some(vec!["rmd".to_string()]),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };

        // Project overrides aliases
        let project_languages = HashMap::from([(
            "markdown".to_string(),
            LanguageSettings {
                aliases: Some(vec!["qmd".to_string(), "mdx".to_string()]),
                ..Default::default()
            },
        )]);

        let project_config = RawWorkspaceSettings {
            languages: project_languages,
            ..Default::default()
        };

        let result = merge_workspace_settings(Some(user_config), Some(project_config));
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
        let merged = merge_workspace_settings(Some(defaults), Some(user_settings));
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
        let map = HashMap::from([
            (
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
            ),
            (
                "python".to_string(),
                settings::BridgeLanguageConfig {
                    enabled: Some(false),
                    aggregation: None,
                },
            ),
        ]);

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
    fn test_merge_language_settings_bridge_aggregation_inherited_from_base() {
        // Base bridge has aggregation, overlay has enabled override but no aggregation.
        // After merge, aggregation should be inherited from base.
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let overlay = LanguageSettings {
            bridge: Some(HashMap::from([(
                "python".to_string(),
                settings::BridgeLanguageConfig {
                    enabled: Some(false),
                    aggregation: None,
                },
            )])),
            ..Default::default()
        };

        let merged = merge_language_settings(&base, &overlay);
        let python = merged.bridge.unwrap().remove("python").unwrap();
        assert_eq!(
            python.enabled,
            Some(false),
            "specific enabled should override"
        );
        assert!(
            python.aggregation.is_some(),
            "aggregation should be inherited from base"
        );
        assert_eq!(
            python.aggregation.as_ref().unwrap()["_"].priorities,
            vec!["pyright".to_string()]
        );
    }

    #[test]
    fn test_merge_language_settings_empty_bridge_overlay_clears_base() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
                "python".to_string(),
                settings::BridgeLanguageConfig {
                    enabled: Some(true),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };

        let overlay = LanguageSettings {
            bridge: Some(HashMap::new()),
            ..Default::default()
        };

        let merged = merge_language_settings(&base, &overlay);
        assert!(
            merged.bridge.unwrap().is_empty(),
            "empty overlay should clear base"
        );
    }

    #[test]
    fn test_merge_language_settings_bridge_aggregation_overlay_wins_on_shared_keys() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let overlay = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let merged = merge_language_settings(&base, &overlay);
        let python = merged.bridge.unwrap().remove("python").unwrap();

        assert_eq!(python.enabled, Some(true));

        let agg = python.aggregation.as_ref().unwrap();
        assert_eq!(
            agg["textDocument/hover"].priorities,
            vec!["overlay_hover".to_string()],
            "overlay should win for shared aggregation keys"
        );
        assert_eq!(
            agg["_"].priorities,
            vec!["base_default".to_string()],
            "base-only aggregation keys should be preserved"
        );
    }

    #[test]
    fn test_merge_language_settings_bridge_aggregation_strategy_replaced_atomically() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let overlay = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let merged = merge_language_settings(&base, &overlay);
        let python = merged.bridge.unwrap().remove("python").unwrap();
        let diag = &python.aggregation.as_ref().unwrap()["textDocument/diagnostic"];

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
    fn test_merge_language_settings_bridge_max_fan_out_preserved_from_base_only_keys() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let overlay = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let merged = merge_language_settings(&base, &overlay);
        let python = merged.bridge.unwrap().remove("python").unwrap();
        let agg = python.aggregation.as_ref().unwrap();

        assert_eq!(
            agg["_"].max_fan_out,
            Some(3),
            "base maxFanOut should be preserved when overlay omits the method key"
        );
    }

    #[test]
    fn test_merge_language_settings_bridge_max_fan_out_lost_on_same_key_replacement() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let overlay = LanguageSettings {
            bridge: Some(HashMap::from([(
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
            )])),
            ..Default::default()
        };

        let merged = merge_language_settings(&base, &overlay);
        let python = merged.bridge.unwrap().remove("python").unwrap();
        let hover = &python.aggregation.as_ref().unwrap()["textDocument/hover"];

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

    // Tests for resolve_base_configs

    #[test]
    fn test_resolve_base_configs_replaces_derived_settings() {
        let languages = HashMap::from([
            (
                "markdown".to_string(),
                LanguageSettings {
                    parser: Some("/opt/markdown.so".to_string()),
                    queries: Some(vec![settings::QueryItem {
                        path: "/opt/markdown/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    }]),
                    bridge: Some(HashMap::from([(
                        "python".to_string(),
                        BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
            (
                "rmd".to_string(),
                LanguageSettings {
                    base: Some("markdown".to_string()),
                    parser: Some("/opt/rmd.so".to_string()),
                    ..Default::default()
                },
            ),
        ]);

        let languages = resolve_base_configs(&languages);

        let rmd = &languages["rmd"];
        // base field preserved
        assert_eq!(rmd.base, Some("markdown".to_string()));
        // parser/queries/bridge replaced with markdown's
        assert_eq!(rmd.parser, Some("/opt/markdown.so".to_string()));
        assert!(rmd.queries.is_some());
        assert!(rmd.bridge.is_some());
        // markdown unchanged
        let md = &languages["markdown"];
        assert_eq!(md.parser, Some("/opt/markdown.so".to_string()));
    }

    #[test]
    fn test_resolve_base_configs_with_missing_base_uses_defaults() {
        let languages = HashMap::from([(
            "rmd".to_string(),
            LanguageSettings {
                base: Some("markdown".to_string()),
                parser: Some("/opt/rmd.so".to_string()),
                ..Default::default()
            },
        )]);

        let languages = resolve_base_configs(&languages);

        let rmd = &languages["rmd"];
        // base's config is default (None for all fields)
        assert_eq!(rmd.parser, None);
        assert_eq!(rmd.queries, None);
        assert_eq!(rmd.bridge, None);
    }

    #[test]
    fn test_resolve_base_configs_skips_self_reference() {
        let languages = HashMap::from([(
            "rmd".to_string(),
            LanguageSettings {
                base: Some("rmd".to_string()),
                parser: Some("/opt/rmd.so".to_string()),
                ..Default::default()
            },
        )]);

        let languages = resolve_base_configs(&languages);

        let rmd = &languages["rmd"];
        // Self-reference should be skipped: original config preserved
        assert_eq!(rmd.parser, Some("/opt/rmd.so".to_string()));
        // base field preserved (coordinator handles user-facing warning)
        assert_eq!(rmd.base, Some("rmd".to_string()));
    }

    #[test]
    fn test_resolve_base_configs_no_base_languages_unchanged() {
        let languages = HashMap::from([(
            "rust".to_string(),
            LanguageSettings {
                parser: Some("/opt/rust.so".to_string()),
                ..Default::default()
            },
        )]);

        let resolved = resolve_base_configs(&languages);
        assert_eq!(resolved, languages);
    }
}
