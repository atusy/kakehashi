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
pub(super) fn merge_bridge_maps(
    base: &Option<HashMap<String, BridgeLanguageConfig>>,
    overlay: &Option<HashMap<String, BridgeLanguageConfig>>,
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

pub(super) fn merge_language_servers(
    base: Option<HashMap<String, BridgeServerConfig>>,
    overlay: Option<HashMap<String, BridgeServerConfig>>,
) -> Option<HashMap<String, BridgeServerConfig>> {
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
