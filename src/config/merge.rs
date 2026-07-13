use super::settings::{
    AggregationConfig, BridgeLanguageConfig, BridgeServerConfig, LanguageSettings,
    LayerAggregationConfig, LayersConfig,
};
use super::{CaptureMappings, RawWorkspaceSettings, WILDCARD_KEY};
use std::collections::{HashMap, HashSet};

/// Resolve a key from a map with wildcard fallback and merging.
///
/// Implements wildcard config inheritance (wildcard-config-inheritance) for HashMap-based settings:
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

/// Cheaply checks whether `name`'s effective config is spawnable (non-empty
/// `cmd` AND enabled), resolving `_` wildcard inheritance via
/// [`BridgeServerConfig::is_spawnable_with_wildcard`] without cloning or
/// merging the full `BridgeServerConfig` — a hot-path-friendly alternative to
/// `resolve_with_wildcard(..., merge_bridge_server_configs).is_some_and(|c|
/// c.is_spawnable())` for call sites that only need the boolean, not the
/// resolved config. A loop over many servers should look up the wildcard once
/// and call `is_spawnable_with_wildcard` directly instead of calling this
/// function per server (which re-looks-up the wildcard every time).
///
/// `name` should be a concrete server key, not the wildcard itself — the
/// wildcard is a template, never a server in its own right, so this always
/// returns `false` for `WILDCARD_KEY` regardless of its own cmd/enabled
/// (enforced, not just documented). An absent name likewise resolves to
/// `false` rather than falling back to the wildcard's own config, since a
/// server no longer listed in `languageServers` at all was never eligible in
/// the first place.
pub(crate) fn is_server_spawnable(
    servers: &HashMap<String, BridgeServerConfig>,
    name: &str,
) -> bool {
    if name == WILDCARD_KEY {
        return false;
    }
    servers
        .get(name)
        .is_some_and(|config| config.is_spawnable_with_wildcard(servers.get(WILDCARD_KEY)))
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
            (Some(_), Some(overlay_agg)) if overlay_agg.is_empty() => Some(HashMap::new()),
            (Some(base_agg), Some(overlay_agg)) => {
                let mut merged = base_agg.clone();
                for (method, overlay_config) in overlay_agg {
                    merged
                        .entry(method.clone())
                        .and_modify(|base_config| {
                            *base_config = merge_aggregation_configs(base_config, overlay_config);
                        })
                        .or_insert_with(|| overlay_config.clone());
                }
                Some(merged)
            }
            (base_agg, overlay_agg) => overlay_agg.clone().or_else(|| base_agg.clone()),
        },
    }
}

/// Field-level merge of two BridgeServerConfig values.
/// Vec fields: use overlay if non-empty, else base.
/// JSON Option fields: deep merge (configuration-merging-strategy).
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
        settings: match (&base.settings, &overlay.settings) {
            (Some(b), Some(o)) => Some(deep_merge_json(b, o)),
            _ => overlay.settings.clone().or(base.settings.clone()),
        },
        workspace_markers: overlay
            .workspace_markers
            .clone()
            .or_else(|| base.workspace_markers.clone()),
        on_type_formatting_triggers: overlay
            .on_type_formatting_triggers
            .clone()
            .or_else(|| base.on_type_formatting_triggers.clone()),
        // Overlay-wins-when-present, mirroring `workspace_markers`: a concrete
        // server's explicit `preferSharedInstance` overrides the wildcard
        // (#391), so `_.preferSharedInstance: true` can be opted out of per
        // server. An unset overlay inherits the base (wildcard) value.
        prefer_shared_instance: overlay
            .prefer_shared_instance
            .or(base.prefer_shared_instance),
        // Overlay-wins-when-present, mirroring `prefer_shared_instance`: a
        // concrete server's explicit `enabled` overrides the wildcard, so
        // `_.enabled: false` can be opted back into per server.
        features: {
            let mut features = base.features.clone();
            for (method, overlay_config) in &overlay.features {
                features
                    .entry(method.clone())
                    .and_modify(|base_config| {
                        base_config.log_level = overlay_config.log_level.or(base_config.log_level);
                    })
                    .or_insert_with(|| overlay_config.clone());
            }
            features
        },
        enabled: overlay.enabled.or(base.enabled),
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
        layers: merge_layers_configs(base.layers.as_ref(), overlay.layers.as_ref()),
        aliases: overlay.aliases.clone().or_else(|| base.aliases.clone()),
    }
}

/// Resolve base configs: for each language, walk the `base` chain and merge
/// configs using most-specific-wins semantics (base-language-inheritance Phase 2).
///
/// The chain is built from most-specific (leaf) to least-specific (root),
/// then merged root-to-leaf so that more specific entries override less
/// specific ones at the field level.
///
/// Chain termination:
/// - Self-reference (`base == own name`): normal termination
/// - `None` base on `_`: terminates (root of all chains)
/// - `None` base on other languages: implicitly chains to `_`
/// - Circular reference: terminated at cycle point with a warning
///
/// The `base` field in the resolved config is restored to the original value
/// so that `LanguageCoordinator`'s loading logic is not affected by inherited
/// `base` values from the chain.
pub(crate) fn resolve_base_configs(
    languages: &HashMap<String, LanguageSettings>,
) -> HashMap<String, LanguageSettings> {
    languages
        .keys()
        .map(|name| {
            let original_base = languages.get(name).and_then(|s| s.base.clone());
            let chain = build_base_chain(name, languages);
            let mut resolved = chain
                .iter()
                .rev()
                .map(|n| languages.get(n).cloned().unwrap_or_default())
                .reduce(|acc, settings| merge_language_settings(&acc, &settings))
                .unwrap_or_default();
            resolved.base = original_base;
            (name.clone(), resolved)
        })
        .collect()
}

/// Build the base chain for a language, from most-specific to least-specific.
///
/// Example: for `rmd` with `base = "markdown"`, and `markdown` with no base,
/// the chain is `["rmd", "markdown", "_"]`.
///
/// Languages with no explicit `base` implicitly chain to `_` (base-language-inheritance).
/// `_` itself terminates when its `base` is `None` or self-referential.
fn build_base_chain(name: &str, languages: &HashMap<String, LanguageSettings>) -> Vec<String> {
    let mut chain = vec![name.to_string()];
    let mut visited = HashSet::new();
    visited.insert(name.to_string());

    let mut current = name.to_string();
    loop {
        let base_name = match languages.get(&current).and_then(|s| s.base.as_deref()) {
            None => {
                if current == WILDCARD_KEY {
                    break; // "_" with no explicit base terminates the chain
                }
                WILDCARD_KEY.to_string() // implicit chain to "_"
            }
            Some(b) => b.to_string(),
        };

        // Self-reference terminates the chain
        if base_name == current {
            break;
        }

        // Cycle detection
        if !visited.insert(base_name.clone()) {
            log::warn!(
                target: "kakehashi::config",
                "Circular base chain detected for language '{}': \
                 '{}' points to '{}' which was already visited. \
                 Wildcard defaults will not be applied.",
                name, current, base_name
            );
            break;
        }

        chain.push(base_name.clone());
        current = base_name;
    }

    chain
}

/// Merge two `AggregationConfig`s field-by-field.
///
/// - `strategy` / `max_fan_out`: overlay wins if set, else inherits from base
/// - `priorities`: overlay wins if present, else inherits from base
pub(crate) fn merge_aggregation_configs(
    base: &AggregationConfig,
    overlay: &AggregationConfig,
) -> AggregationConfig {
    AggregationConfig {
        strategy: overlay.strategy.or(base.strategy),
        priorities: overlay
            .priorities
            .clone()
            .or_else(|| base.priorities.clone()),
        max_fan_out: overlay.max_fan_out.or(base.max_fan_out),
        pull_fallback: overlay.pull_fallback.or(base.pull_fallback),
        push_fallback: overlay.push_fallback.or(base.push_fallback),
    }
}

/// Merge two `LayerAggregationConfig`s field-by-field
/// (cross-layer-aggregation).
///
/// - `priorities`: overlay wins if present, else inherits from base — the
///   list replaces wholesale, never element-wise
/// - `strategy`: overlay wins if set, else inherits from base
pub(crate) fn merge_layer_aggregation_configs(
    base: &LayerAggregationConfig,
    overlay: &LayerAggregationConfig,
) -> LayerAggregationConfig {
    LayerAggregationConfig {
        priorities: overlay
            .priorities
            .clone()
            .or_else(|| base.priorities.clone()),
        strategy: overlay.strategy.or(base.strategy),
    }
}

/// Field-level merge of two optional `layers` configs
/// (cross-layer-aggregation).
fn merge_layers_configs(
    base: Option<&LayersConfig>,
    overlay: Option<&LayersConfig>,
) -> Option<LayersConfig> {
    match (base, overlay) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(b), Some(o)) => Some(LayersConfig {
            aggregation: merge_layers_aggregation_maps(
                b.aggregation.as_ref(),
                o.aggregation.as_ref(),
            ),
        }),
    }
}

/// Deep merge two optional `layers.aggregation` HashMaps
/// (cross-layer-aggregation).
///
/// Mirrors [`merge_bridge_maps`]: an empty overlay map (`Some({})`) clears
/// the base (empty-means-clear); otherwise per-method entries merge at the
/// field level via [`merge_layer_aggregation_configs`].
fn merge_layers_aggregation_maps(
    base: Option<&HashMap<String, LayerAggregationConfig>>,
    overlay: Option<&HashMap<String, LayerAggregationConfig>>,
) -> Option<HashMap<String, LayerAggregationConfig>> {
    match (base, overlay) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(o)) => Some(o.clone()),
        (Some(_), Some(o)) if o.is_empty() => Some(HashMap::new()), // empty-means-clear
        (Some(b), Some(o)) => {
            let mut merged = b.clone();
            for (method, overlay_config) in o {
                merged
                    .entry(method.clone())
                    .and_modify(|base_config| {
                        *base_config = merge_layer_aggregation_configs(base_config, overlay_config);
                    })
                    .or_insert_with(|| overlay_config.clone());
            }
            Some(merged)
        }
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

/// Deep merge two JSON values (configuration-merging-strategy).
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
                diagnostics_debounce_ms: overlay
                    .diagnostics_debounce_ms
                    .or(base.diagnostics_debounce_ms),
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
            diagnostics_debounce_ms: None,
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
                        workspace_markers: None,
                        on_type_formatting_triggers: None,
                        prefer_shared_instance: None,
                        features: Default::default(),
                        enabled: None,
                        settings: None,
                    },
                ),
                (
                    "lua-language-server".to_string(),
                    BridgeServerConfig {
                        cmd: vec!["lua-language-server".to_string()],
                        languages: vec!["lua".to_string()],
                        initialization_options: None,
                        workspace_markers: None,
                        on_type_formatting_triggers: None,
                        prefer_shared_instance: None,
                        features: Default::default(),
                        enabled: None,
                        settings: None,
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
            diagnostics_debounce_ms: None,
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
                        workspace_markers: None,
                        on_type_formatting_triggers: None,
                        prefer_shared_instance: None,
                        features: Default::default(),
                        enabled: None,
                        settings: None,
                    },
                ),
                (
                    // new key: pyright — added by overlay
                    "pyright".to_string(),
                    BridgeServerConfig {
                        cmd: vec!["pyright-langserver".to_string(), "--stdio".to_string()],
                        languages: vec!["python".to_string()],
                        initialization_options: None,
                        workspace_markers: None,
                        on_type_formatting_triggers: None,
                        prefer_shared_instance: None,
                        features: Default::default(),
                        enabled: None,
                        settings: None,
                    },
                ),
            ])),
            capture_mappings: HashMap::from([
                (
                    // shared key: _ — overlay overrides variable.builtin, adds type.builtin;
                    //   adds folds fold.function
                    "_".to_string(),
                    QueryTypeMappings {
                        highlights: HashMap::from([
                            (
                                "variable.builtin".to_string(),
                                "overlay.variable".to_string(),
                            ),
                            ("type.builtin".to_string(), "overlay.type".to_string()),
                        ]),
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
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
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

    // Languages wildcard inheritance (wildcard-config-inheritance)

    #[test]
    fn test_specific_values_override_wildcards_at_both_levels() {
        // wildcard-config-inheritance: python.bridge.javascript overrides _.bridge._ settings
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
        // wildcard-config-inheritance: bridge maps are deep merged, so python gets wildcard's bridge._
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
        // wildcard-config-inheritance: Test case where python.bridge includes _ wildcard
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
        // wildcard-config-inheritance: Nested wildcard resolution applies outer then inner
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
        // wildcard-config-inheritance: Bridge maps should be deep merged, not overridden
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

    // languageServers wildcard inheritance (wildcard-config-inheritance)

    /// wildcard-config-inheritance: resolve_with_wildcard covers all 4 match arms for language servers.
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
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                features: Default::default(),
                enabled: None,
                settings: None,
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
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                features: Default::default(),
                enabled: None,
                settings: None,
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
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
                },
            ),
            (
                "ra".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec![],
                    initialization_options: Some(json!({"linkedProjects": ["./Cargo.toml"]})),
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
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

    /// workspaceMarkers merges overlay-wins like other Option fields, and the
    /// explicit `Some([])` kill switch must survive the merge — collapsing
    /// it to "inherit" would silently re-enable the marker search a user
    /// turned off per server.
    #[test]
    fn test_merge_bridge_server_configs_workspace_markers() {
        use settings::{BridgeServerConfig, RootMarker};

        let base = BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: None,
            workspace_markers: Some(vec![RootMarker::Single(".git".to_string())]),
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            features: Default::default(),
            enabled: None,
            settings: None,
        };

        // Unset overlay inherits from base (wildcard default applies)
        let inheriting = BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec!["rust".to_string()],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            features: Default::default(),
            enabled: None,
            settings: None,
        };
        let merged = merge_bridge_server_configs(&base, &inheriting);
        assert_eq!(
            merged.workspace_markers,
            Some(vec![RootMarker::Single(".git".to_string())])
        );

        // Set overlay wins over base
        let overriding = BridgeServerConfig {
            workspace_markers: Some(vec![RootMarker::Single("Cargo.toml".to_string())]),
            ..inheriting.clone()
        };
        let merged = merge_bridge_server_configs(&base, &overriding);
        assert_eq!(
            merged.workspace_markers,
            Some(vec![RootMarker::Single("Cargo.toml".to_string())])
        );

        // Explicit [] survives as the per-server kill switch
        let disabling = BridgeServerConfig {
            workspace_markers: Some(vec![]),
            ..inheriting
        };
        let merged = merge_bridge_server_configs(&base, &disabling);
        assert_eq!(merged.workspace_markers, Some(vec![]));
    }

    /// `preferSharedInstance` merges overlay-wins-when-present like
    /// `workspace_markers`, so a `languageServers._` opt-in applies to servers that
    /// stay unset, yet a concrete server can override it either direction —
    /// crucially opting **out** of a blanket `_.preferSharedInstance: true`
    /// with an explicit `false` (#391).
    #[test]
    fn test_merge_bridge_server_configs_prefer_shared_instance() {
        use settings::BridgeServerConfig;

        let server = |prefer: Option<bool>| BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: prefer,
            settings: None,
            features: Default::default(),
            enabled: None,
        };

        // Unset overlay inherits the base (wildcard) value.
        let base = server(Some(true));
        assert_eq!(
            merge_bridge_server_configs(&base, &server(None)).prefer_shared_instance,
            Some(true),
            "unset overlay inherits the wildcard opt-in"
        );

        // Explicit overlay overrides the base — including opting OUT of a
        // wildcard opt-in.
        assert_eq!(
            merge_bridge_server_configs(&base, &server(Some(false))).prefer_shared_instance,
            Some(false),
            "explicit false opts a server out of the wildcard opt-in"
        );

        // And opting in over a wildcard that left it unset.
        assert_eq!(
            merge_bridge_server_configs(&server(None), &server(Some(true))).prefer_shared_instance,
            Some(true),
        );
    }

    /// `enabled` merges overlay-wins-when-present, exactly like
    /// `prefer_shared_instance`, so a `languageServers._.enabled: false` can
    /// disable every server by default while a concrete server opts back in
    /// with `enabled: true`.
    #[test]
    fn test_merge_bridge_server_configs_enabled() {
        use settings::BridgeServerConfig;

        let server = |enabled: Option<bool>| BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            settings: None,
            features: Default::default(),
            enabled,
        };

        // Unset overlay inherits the base (wildcard) value.
        let base = server(Some(false));
        assert_eq!(
            merge_bridge_server_configs(&base, &server(None)).enabled,
            Some(false),
            "unset overlay inherits the wildcard opt-out"
        );

        // Explicit overlay overrides the base — opting a specific server
        // BACK IN over a wildcard opt-out.
        assert_eq!(
            merge_bridge_server_configs(&base, &server(Some(true))).enabled,
            Some(true),
            "explicit true opts a server back in over the wildcard opt-out"
        );

        // And opting a specific server out over a wildcard that left it unset.
        assert_eq!(
            merge_bridge_server_configs(&server(None), &server(Some(false))).enabled,
            Some(false),
        );
    }

    #[test]
    fn test_merge_bridge_server_configs_features_by_method_and_field() {
        use settings::{BridgeServerConfig, ForwardLogLevel, MethodForwardingConfig};

        let base = BridgeServerConfig {
            features: HashMap::from([
                (
                    "window/logMessage".to_string(),
                    MethodForwardingConfig {
                        log_level: Some(ForwardLogLevel::Warning),
                    },
                ),
                (
                    "other/method".to_string(),
                    MethodForwardingConfig::default(),
                ),
            ]),
            ..Default::default()
        };
        let overlay = BridgeServerConfig {
            features: HashMap::from([(
                "window/logMessage".to_string(),
                MethodForwardingConfig {
                    log_level: Some(ForwardLogLevel::Info),
                },
            )]),
            ..Default::default()
        };

        let merged = merge_bridge_server_configs(&base, &overlay);
        assert_eq!(
            merged.features["window/logMessage"].log_level,
            Some(ForwardLogLevel::Info)
        );
        assert!(merged.features.contains_key("other/method"));
    }

    // Deep merge for initialization_options (configuration-merging-strategy)

    /// configuration-merging-strategy: initialization_options deep merge covers three behaviors:
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
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            features: Default::default(),
            enabled: None,
            settings: None,
        };
        let overlay = BridgeServerConfig {
            cmd: vec!["rust-analyzer".to_string()],
            languages: vec!["rust".to_string()],
            initialization_options: Some(json!({
                "feature2": true,
                "shared_opt": "overlay",
                "nested": { "overlay_only": 2, "shared": "overlay" }
            })),
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            features: Default::default(),
            enabled: None,
            settings: None,
        };

        let resolved = merge_bridge_server_configs(&base, &overlay);
        let mut snap_settings = insta::Settings::clone_current();
        snap_settings.set_sort_maps(true);
        snap_settings.bind(|| {
            insta::assert_json_snapshot!(resolved.initialization_options);
        });
    }

    #[test]
    fn test_merge_bridge_server_configs_settings_deep_merge() {
        use serde_json::json;
        use settings::BridgeServerConfig;

        // `settings` composes across config layers exactly like
        // `initialization_options`: nested objects deep-merge, scalars take the
        // overlay (downstream-settings-propagation).
        let base = BridgeServerConfig {
            settings: Some(json!({
                "rust-analyzer": {
                    "cargo": { "features": "all", "noDefaultFeatures": false },
                    "check": { "command": "clippy" }
                }
            })),
            ..Default::default()
        };
        let overlay = BridgeServerConfig {
            settings: Some(json!({
                "rust-analyzer": {
                    "cargo": { "features": ["foo"] }
                }
            })),
            ..Default::default()
        };

        let resolved = merge_bridge_server_configs(&base, &overlay);

        assert_eq!(
            resolved.settings,
            Some(json!({
                "rust-analyzer": {
                    "cargo": { "features": ["foo"], "noDefaultFeatures": false },
                    "check": { "command": "clippy" }
                }
            })),
            "overlay scalar replaces, sibling keys are preserved by deep merge"
        );
    }

    #[test]
    fn test_merge_bridge_server_configs_settings_overlay_or_base_when_one_side_none() {
        use serde_json::json;
        use settings::BridgeServerConfig;

        let with_settings = BridgeServerConfig {
            settings: Some(json!({ "Lua": { "diagnostics": { "globals": ["vim"] } } })),
            ..Default::default()
        };
        let without = BridgeServerConfig::default();

        // base has settings, overlay does not → base survives.
        assert_eq!(
            merge_bridge_server_configs(&with_settings, &without).settings,
            with_settings.settings,
        );
        // overlay has settings, base does not → overlay wins.
        assert_eq!(
            merge_bridge_server_configs(&without, &with_settings).settings,
            with_settings.settings,
        );
    }

    /// `onTypeFormattingTriggers` merges overlay-wins-when-present, so a
    /// wildcard `languageServers._` default applies unless the server entry
    /// overrides it.
    #[test]
    fn test_merge_bridge_server_configs_on_type_formatting_triggers_overlay_wins() {
        use settings::BridgeServerConfig;

        let server = |triggers: Option<Vec<&str>>| BridgeServerConfig {
            cmd: vec![],
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: triggers
                .map(|t| t.into_iter().map(String::from).collect()),
            prefer_shared_instance: None,
            features: Default::default(),
            enabled: None,
            settings: None,
        };

        let base = server(Some(vec!["}"]));
        assert_eq!(
            merge_bridge_server_configs(&base, &server(None)).on_type_formatting_triggers,
            Some(vec!["}".to_string()]),
            "unset overlay inherits the base (wildcard) value"
        );
        assert_eq!(
            merge_bridge_server_configs(&base, &server(Some(vec![";"])))
                .on_type_formatting_triggers,
            Some(vec![";".to_string()]),
            "explicit overlay replaces the base list (no union at merge level)"
        );
    }

    // Wildcard config resolution tests

    /// Verifies that languages._ (wildcard) settings are inherited
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

    /// Verifies that when we look up host language settings using
    /// WorkspaceSettings.languages (HashMap<String, LanguageSettings>),
    /// wildcard resolution is applied so that undefined languages inherit
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

    /// Verifies that when looking up a language server config by name,
    /// the wildcard server settings (languageServers._) are merged with
    /// specific server settings.
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
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
                },
            ),
            // rust-analyzer: only specifies cmd and languages
            (
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec!["rust".to_string()],
                    initialization_options: None, // Should inherit from wildcard
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
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
    /// wildcard-config-inheritance: When languageServers.rust-analyzer has empty languages but
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
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
                },
            ),
            // rust-analyzer: specifies only cmd, inherits languages from wildcard
            (
                "rust-analyzer".to_string(),
                BridgeServerConfig {
                    cmd: vec!["rust-analyzer".to_string()],
                    languages: vec![], // Empty - should inherit from wildcard
                    initialization_options: None,
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    features: Default::default(),
                    enabled: None,
                    settings: None,
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
                            priorities: Some(vec!["server_a".to_string()]),
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
            Some(vec!["server_a".to_string()])
        );
    }

    /// `is_server_spawnable` is a hot-path shortcut for exactly what
    /// `resolve_with_wildcard(..., merge_bridge_server_configs).is_some_and(|c|
    /// c.is_spawnable())` computes — verify the two agree across the cases
    /// that actually exercise wildcard inheritance (cmd AND enabled, each
    /// independently), not just the trivial no-inheritance case.
    #[test]
    fn test_is_server_spawnable_matches_full_resolution() {
        let server = |cmd: Vec<&str>, enabled: Option<bool>| settings::BridgeServerConfig {
            cmd: cmd.into_iter().map(String::from).collect(),
            languages: vec![],
            initialization_options: None,
            workspace_markers: None,
            on_type_formatting_triggers: None,
            prefer_shared_instance: None,
            enabled,
            settings: None,
            features: Default::default(),
        };

        let full_resolution = |servers: &HashMap<String, settings::BridgeServerConfig>,
                               name: &str| {
            resolve_with_wildcard(servers, name, merge_bridge_server_configs)
                .is_some_and(|c| c.is_spawnable())
        };

        // Case 1: server has its own cmd and enabled — no inheritance needed.
        let servers = HashMap::from([("a".to_string(), server(vec!["x"], Some(true)))]);
        assert!(is_server_spawnable(&servers, "a"));
        assert_eq!(
            is_server_spawnable(&servers, "a"),
            full_resolution(&servers, "a")
        );

        // Case 2: server's own cmd is empty, inherits a non-empty cmd from `_`.
        let servers = HashMap::from([
            ("_".to_string(), server(vec!["shared-ls"], None)),
            ("a".to_string(), server(vec![], None)),
        ]);
        assert!(
            is_server_spawnable(&servers, "a"),
            "cmd inherited from the wildcard must count as non-empty"
        );
        assert_eq!(
            is_server_spawnable(&servers, "a"),
            full_resolution(&servers, "a")
        );

        // Case 3: server has its own cmd but the wildcard disables everything
        // by default; server doesn't override enabled — inherits disabled.
        let servers = HashMap::from([
            ("_".to_string(), server(vec![], Some(false))),
            ("a".to_string(), server(vec!["x"], None)),
        ]);
        assert!(!is_server_spawnable(&servers, "a"));
        assert_eq!(
            is_server_spawnable(&servers, "a"),
            full_resolution(&servers, "a")
        );

        // Case 4: server re-enables itself over a disabled wildcard.
        let servers = HashMap::from([
            ("_".to_string(), server(vec![], Some(false))),
            ("a".to_string(), server(vec!["x"], Some(true))),
        ]);
        assert!(is_server_spawnable(&servers, "a"));
        assert_eq!(
            is_server_spawnable(&servers, "a"),
            full_resolution(&servers, "a")
        );

        // Case 5: name not present in the map at all — false, not a fallback
        // to the wildcard's own config.
        let servers = HashMap::from([("_".to_string(), server(vec!["x"], Some(true)))]);
        assert!(!is_server_spawnable(&servers, "missing"));

        // Case 6: called with the wildcard key itself — false, per the
        // documented contract (the wildcard is a template, never a server in
        // its own right), even when it has a non-empty cmd and enabled: true.
        let servers = HashMap::from([("_".to_string(), server(vec!["x"], Some(true)))]);
        assert!(
            !is_server_spawnable(&servers, WILDCARD_KEY),
            "the wildcard key itself must never report as spawnable"
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
                            priorities: Some(vec!["pyright".to_string()]),
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
            Some(vec!["pyright".to_string()])
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
                                priorities: Some(vec!["base_default".to_string()]),
                                ..Default::default()
                            },
                        ),
                        (
                            "textDocument/hover".to_string(),
                            settings::AggregationConfig {
                                priorities: Some(vec!["base_hover".to_string()]),
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
                            priorities: Some(vec!["overlay_hover".to_string()]),
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
            Some(vec!["overlay_hover".to_string()]),
            "overlay should win for shared aggregation keys"
        );
        assert_eq!(
            agg["_"].priorities,
            Some(vec!["base_default".to_string()]),
            "base-only aggregation keys should be preserved"
        );
    }

    #[test]
    fn test_merge_language_settings_bridge_aggregation_strategy_inherited_on_same_key() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
                "python".to_string(),
                settings::BridgeLanguageConfig {
                    enabled: Some(true),
                    aggregation: Some(HashMap::from([(
                        "textDocument/diagnostic".to_string(),
                        settings::AggregationConfig {
                            priorities: Some(vec!["ruff".to_string()]),
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
                            priorities: Some(vec!["pyright".to_string()]),
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
            Some(vec!["pyright".to_string()]),
            "overlay priorities should win"
        );
        assert_eq!(
            diag.strategy,
            Some(settings::AggregationStrategy::Concatenated),
            "base strategy should be inherited when overlay omits it"
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
                            priorities: Some(vec!["pyright".to_string()]),
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
    fn test_merge_language_settings_bridge_max_fan_out_inherited_on_same_key() {
        let base = LanguageSettings {
            bridge: Some(HashMap::from([(
                "python".to_string(),
                settings::BridgeLanguageConfig {
                    enabled: Some(true),
                    aggregation: Some(HashMap::from([(
                        "textDocument/hover".to_string(),
                        settings::AggregationConfig {
                            priorities: Some(vec!["ruff".to_string()]),
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
                            priorities: Some(vec!["pyright".to_string()]),
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
            Some(vec!["pyright".to_string()]),
            "overlay priorities should win"
        );
        assert_eq!(
            hover.max_fan_out,
            Some(2),
            "base maxFanOut should be inherited when overlay omits it"
        );
    }

    // Tests for resolve_base_configs

    #[test]
    fn test_resolve_base_configs_most_specific_wins() {
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
        // rmd's own parser wins (most-specific-wins)
        assert_eq!(rmd.parser, Some("/opt/rmd.so".to_string()));
        // queries/bridge inherited from markdown (rmd didn't set them)
        assert!(rmd.queries.is_some());
        assert!(rmd.bridge.is_some());
        // markdown unchanged
        let md = &languages["markdown"];
        assert_eq!(md.parser, Some("/opt/markdown.so".to_string()));
    }

    #[test]
    fn test_resolve_base_configs_inherits_from_base_when_none() {
        let languages = HashMap::from([
            (
                "markdown".to_string(),
                LanguageSettings {
                    parser: Some("/opt/markdown.so".to_string()),
                    queries: Some(vec![settings::QueryItem {
                        path: "/opt/markdown/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            ),
            (
                "rmd".to_string(),
                LanguageSettings {
                    base: Some("markdown".to_string()),
                    // no parser, no queries — should inherit from markdown
                    ..Default::default()
                },
            ),
        ]);

        let languages = resolve_base_configs(&languages);

        let rmd = &languages["rmd"];
        assert_eq!(rmd.parser, Some("/opt/markdown.so".to_string()));
        assert!(rmd.queries.is_some());
    }

    #[test]
    fn test_resolve_base_configs_with_missing_base_preserves_own() {
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
        // markdown not in map → defaults (all None), rmd's own parser wins
        assert_eq!(rmd.parser, Some("/opt/rmd.so".to_string()));
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

    #[test]
    fn test_resolve_base_configs_multi_level_chain() {
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
                "markdown_custom".to_string(),
                LanguageSettings {
                    base: Some("markdown".to_string()),
                    queries: Some(vec![settings::QueryItem {
                        path: "/opt/custom/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            ),
            (
                "rmd".to_string(),
                LanguageSettings {
                    base: Some("markdown_custom".to_string()),
                    // rmd → markdown_custom → markdown
                    ..Default::default()
                },
            ),
        ]);

        let languages = resolve_base_configs(&languages);

        let rmd = &languages["rmd"];
        // parser from markdown (root of chain)
        assert_eq!(rmd.parser, Some("/opt/markdown.so".to_string()));
        // queries from markdown_custom (more specific than markdown)
        assert_eq!(
            rmd.queries.as_ref().unwrap()[0].path,
            "/opt/custom/highlights.scm"
        );
        // bridge from markdown
        assert!(rmd.bridge.is_some());
    }

    #[test]
    fn test_resolve_base_configs_circular_reference() {
        let languages = HashMap::from([
            (
                "a".to_string(),
                LanguageSettings {
                    base: Some("b".to_string()),
                    parser: Some("/opt/a.so".to_string()),
                    ..Default::default()
                },
            ),
            (
                "b".to_string(),
                LanguageSettings {
                    base: Some("a".to_string()),
                    queries: Some(vec![settings::QueryItem {
                        path: "/opt/b/highlights.scm".to_string(),
                        kind: Some(settings::QueryKind::Highlights),
                    }]),
                    ..Default::default()
                },
            ),
        ]);

        let languages = resolve_base_configs(&languages);

        // Both languages should still resolve (cycle is broken, not a panic)
        assert!(languages.contains_key("a"));
        assert!(languages.contains_key("b"));
        // a's chain: [a, b] (stops at cycle), merged: b then a
        let a = &languages["a"];
        assert_eq!(a.parser, Some("/opt/a.so".to_string()));
        assert!(a.queries.is_some()); // inherited from b
    }

    #[test]
    fn test_resolve_base_configs_wildcard_self_reference_terminates() {
        let languages = HashMap::from([
            (
                "_".to_string(),
                LanguageSettings {
                    base: Some("_".to_string()),
                    parser: Some("/opt/default.so".to_string()),
                    ..Default::default()
                },
            ),
            (
                "rust".to_string(),
                LanguageSettings {
                    base: Some("_".to_string()),
                    parser: Some("/opt/rust.so".to_string()),
                    ..Default::default()
                },
            ),
        ]);

        let languages = resolve_base_configs(&languages);

        // _ self-reference: chain is just ["_"], no infinite loop
        let wildcard = &languages["_"];
        assert_eq!(wildcard.parser, Some("/opt/default.so".to_string()));
        // rust -> _: chain is ["rust", "_"], rust's parser wins
        let rust = &languages["rust"];
        assert_eq!(rust.parser, Some("/opt/rust.so".to_string()));
    }

    #[test]
    fn test_resolve_base_configs_none_base_chains_to_wildcard() {
        // A language with base: None should implicitly chain to "_" and inherit
        // its settings when "_" exists in the map. The language's own base field
        // must remain None after resolution (not inherit "_"'s self-ref).
        let languages = HashMap::from([
            (
                WILDCARD_KEY.to_string(),
                LanguageSettings {
                    base: Some(WILDCARD_KEY.to_string()), // self-reference
                    parser: Some("/opt/default.so".to_string()),
                    bridge: Some(HashMap::from([(
                        WILDCARD_KEY.to_string(),
                        BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
            (
                "python".to_string(),
                LanguageSettings {
                    // base: None — should implicitly chain to "_"
                    parser: Some("/opt/python.so".to_string()),
                    ..Default::default()
                },
            ),
        ]);

        let resolved = resolve_base_configs(&languages);
        let python = &resolved["python"];

        // parser: python's own wins (most-specific)
        assert_eq!(python.parser, Some("/opt/python.so".to_string()));
        // bridge: inherited from "_"
        assert!(
            python.bridge.is_some(),
            "python should inherit bridge from '_'"
        );
        // base: original value preserved, not inherited from "_"
        assert_eq!(python.base, None, "python's base field should remain None");
    }

    #[test]
    fn test_resolve_base_configs_wildcard_none_base_terminates() {
        // "_" with base: None should not cause infinite loop — it terminates.
        let languages = HashMap::from([(
            WILDCARD_KEY.to_string(),
            LanguageSettings {
                // base: None — should be treated as self-reference for "_"
                parser: Some("/opt/default.so".to_string()),
                ..Default::default()
            },
        )]);

        // Must not hang or panic
        let resolved = resolve_base_configs(&languages);
        assert_eq!(
            resolved[WILDCARD_KEY].parser,
            Some("/opt/default.so".to_string())
        );
    }

    #[test]
    fn test_resolve_base_configs_self_reference_prevents_wildcard_inheritance() {
        // A language with base = its own name (self-reference) terminates the
        // chain before reaching "_", acting as a blank-slate root.
        // Languages derived from it inherit its config, not "_"'s.
        let languages = HashMap::from([
            (
                WILDCARD_KEY.to_string(),
                LanguageSettings {
                    base: Some(WILDCARD_KEY.to_string()),
                    bridge: Some(HashMap::from([(
                        WILDCARD_KEY.to_string(),
                        BridgeLanguageConfig {
                            enabled: Some(true),
                            ..Default::default()
                        },
                    )])),
                    ..Default::default()
                },
            ),
            (
                "_blank".to_string(),
                LanguageSettings {
                    base: Some("_blank".to_string()), // self-ref: chain stops here
                    ..Default::default()
                },
            ),
            (
                "custom_lang".to_string(),
                LanguageSettings {
                    base: Some("_blank".to_string()),
                    ..Default::default()
                },
            ),
        ]);

        let resolved = resolve_base_configs(&languages);
        let custom = &resolved["custom_lang"];

        // _blank has no bridge, so custom_lang should not inherit "_"'s bridge
        assert!(
            custom.bridge.is_none(),
            "custom_lang should not inherit '_'s bridge — chain stopped at _blank"
        );
    }

    #[test]
    fn merge_layer_aggregation_configs_overlay_fields_win() {
        use crate::config::settings::{AggregationStrategy, LayerSource};
        let base = LayerAggregationConfig {
            priorities: Some(vec![LayerSource::Native]),
            strategy: Some(AggregationStrategy::Preferred),
        };
        let overlay = LayerAggregationConfig {
            priorities: Some(vec![LayerSource::Virt, LayerSource::Host]),
            strategy: None,
        };
        let merged = merge_layer_aggregation_configs(&base, &overlay);
        assert_eq!(
            merged.priorities,
            Some(vec![LayerSource::Virt, LayerSource::Host]),
            "overlay priorities replace base wholesale"
        );
        assert_eq!(
            merged.strategy,
            Some(AggregationStrategy::Preferred),
            "unset overlay strategy inherits from base"
        );
    }

    #[test]
    fn merge_language_settings_merges_layers_per_method() {
        use crate::config::settings::{AggregationStrategy, LayerSource};
        let base = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([
                    (
                        "textDocument/hover".to_string(),
                        LayerAggregationConfig {
                            priorities: Some(vec![LayerSource::Native]),
                            strategy: Some(AggregationStrategy::Preferred),
                        },
                    ),
                    (
                        "textDocument/definition".to_string(),
                        LayerAggregationConfig {
                            priorities: Some(vec![LayerSource::Virt]),
                            strategy: None,
                        },
                    ),
                ])),
            }),
            ..Default::default()
        };
        let overlay = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([(
                    "textDocument/hover".to_string(),
                    LayerAggregationConfig {
                        priorities: Some(vec![LayerSource::Host]),
                        strategy: None,
                    },
                )])),
            }),
            ..Default::default()
        };
        let merged = merge_language_settings(&base, &overlay);
        let aggregation = merged
            .layers
            .expect("layers must survive the merge")
            .aggregation
            .expect("aggregation must survive the merge");
        assert_eq!(
            aggregation["textDocument/hover"].priorities,
            Some(vec![LayerSource::Host]),
            "overlay entry wins per field"
        );
        assert_eq!(
            aggregation["textDocument/hover"].strategy,
            Some(AggregationStrategy::Preferred),
            "unset overlay field inherits from the base entry"
        );
        assert_eq!(
            aggregation["textDocument/definition"].priorities,
            Some(vec![LayerSource::Virt]),
            "base-only entries are preserved"
        );
    }

    #[test]
    fn merge_language_settings_empty_layers_map_clears_base() {
        use crate::config::settings::LayerSource;
        let base = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::from([(
                    "_".to_string(),
                    LayerAggregationConfig {
                        priorities: Some(vec![LayerSource::Native]),
                        strategy: None,
                    },
                )])),
            }),
            ..Default::default()
        };
        let overlay = LanguageSettings {
            layers: Some(LayersConfig {
                aggregation: Some(HashMap::new()),
            }),
            ..Default::default()
        };
        let merged = merge_language_settings(&base, &overlay);
        assert_eq!(
            merged.layers,
            Some(LayersConfig {
                aggregation: Some(HashMap::new()),
            }),
            "aggregation = {{}} clears inherited entries (empty-means-clear, like bridge)"
        );
    }

    #[test]
    fn merge_aggregation_configs_overlay_strategy_wins() {
        let base = settings::AggregationConfig {
            strategy: Some(settings::AggregationStrategy::Preferred),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig {
            strategy: Some(settings::AggregationStrategy::Concatenated),
            ..Default::default()
        };
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(
            merged.strategy,
            Some(settings::AggregationStrategy::Concatenated)
        );
    }

    #[test]
    fn merge_aggregation_configs_inherits_strategy_from_base() {
        let base = settings::AggregationConfig {
            strategy: Some(settings::AggregationStrategy::Preferred),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig::default(); // strategy: None
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(
            merged.strategy,
            Some(settings::AggregationStrategy::Preferred)
        );
    }

    #[test]
    fn merge_aggregation_configs_non_empty_priorities_win() {
        let base = settings::AggregationConfig {
            priorities: Some(vec!["server_base".to_string()]),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig {
            priorities: Some(vec!["server_overlay".to_string()]),
            ..Default::default()
        };
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(merged.priorities, Some(vec!["server_overlay".to_string()]));
    }

    #[test]
    fn merge_aggregation_configs_none_priorities_inherit_from_base() {
        let base = settings::AggregationConfig {
            priorities: Some(vec!["server_base".to_string()]),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig::default(); // priorities: None (inherit)
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(merged.priorities, Some(vec!["server_base".to_string()]));
    }

    #[test]
    fn merge_aggregation_configs_explicit_empty_priorities_clear_base() {
        let base = settings::AggregationConfig {
            priorities: Some(vec!["server_base".to_string()]),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig {
            priorities: Some(vec![]),
            ..Default::default()
        };
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(merged.priorities, Some(vec![]));
    }

    #[test]
    fn merge_bridge_language_configs_empty_aggregation_clears_base() {
        let base = settings::BridgeLanguageConfig {
            aggregation: Some(HashMap::from([(
                "textDocument/hover".to_string(),
                settings::AggregationConfig {
                    strategy: Some(settings::AggregationStrategy::Preferred),
                    ..Default::default()
                },
            )])),
            ..Default::default()
        };
        let overlay = settings::BridgeLanguageConfig {
            aggregation: Some(HashMap::new()),
            ..Default::default()
        };

        let merged = merge_bridge_language_configs(&base, &overlay);

        assert_eq!(merged.aggregation, Some(HashMap::new()));
    }

    #[test]
    fn merge_aggregation_configs_max_fan_out_overlay_wins() {
        let base = settings::AggregationConfig {
            max_fan_out: Some(3),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig {
            max_fan_out: Some(7),
            ..Default::default()
        };
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(merged.max_fan_out, Some(7));
    }

    #[test]
    fn merge_aggregation_configs_max_fan_out_inherits_from_base() {
        let base = settings::AggregationConfig {
            max_fan_out: Some(5),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig::default(); // max_fan_out: None
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(merged.max_fan_out, Some(5));
    }

    #[test]
    fn merge_aggregation_configs_fallback_toggles_overlay_wins() {
        let base = settings::AggregationConfig {
            pull_fallback: Some(true),
            push_fallback: Some(true),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig {
            pull_fallback: Some(false),
            push_fallback: Some(false),
            ..Default::default()
        };
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(merged.pull_fallback, Some(false));
        assert_eq!(merged.push_fallback, Some(false));
    }

    #[test]
    fn merge_aggregation_configs_fallback_toggles_inherit_from_base() {
        // Each toggle merges independently: an unset overlay field inherits the
        // base while a set one (here `push_fallback`) overrides.
        let base = settings::AggregationConfig {
            pull_fallback: Some(false),
            push_fallback: Some(false),
            ..Default::default()
        };
        let overlay = settings::AggregationConfig {
            push_fallback: Some(true),
            ..Default::default()
        };
        let merged = merge_aggregation_configs(&base, &overlay);
        assert_eq!(
            merged.pull_fallback,
            Some(false),
            "unset overlay inherits base"
        );
        assert_eq!(
            merged.push_fallback,
            Some(true),
            "set overlay overrides base"
        );
    }
}
