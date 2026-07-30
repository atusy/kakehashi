//! Recognising configuration keys kakehashi does not know.
//!
//! Serde ignores an unknown field silently, so a typo in a configuration file
//! reads as "this setting was not specified" rather than as a mistake. Both the
//! runtime `workspace/didChangeConfiguration` path and explicit `--config-file`
//! loading walk the incoming value against these known-key sets; what each does
//! with the answer is that caller's policy — the former rejects the update, the
//! latter warns.
//!
//! The walk does not cover `features`: `FeatureSettings` and its children carry
//! `deny_unknown_fields`, so an unknown key there fails typed deserialization
//! before any of this runs. That makes it fatal on a config file, unlike every
//! other unknown key, which is a wrinkle worth removing rather than a design.

use serde_json::Value;

pub(crate) const KNOWN_WORKSPACE_SETTING_KEYS: &[&str] = &[
    "searchPaths",
    "languages",
    "captureMappings",
    "autoInstall",
    "diagnosticsDebounceMs",
    "features",
    "languageServers",
];

pub(crate) const KNOWN_FEATURE_SETTING_KEYS: &[&str] = &[
    "textDocument/publishDiagnostics",
    "window/logMessage",
    "workspace/diagnostic/refresh",
];

pub(crate) const KNOWN_AGGREGATION_SETTING_KEYS: &[&str] = &[
    "maxFanOut",
    "priorities",
    "pullFallback",
    "pushFallback",
    "strategy",
];

pub(crate) const KNOWN_BRIDGE_LANGUAGE_SETTING_KEYS: &[&str] = &["aggregation", "enabled"];

pub(crate) const KNOWN_BRIDGE_SERVER_SETTING_KEYS: &[&str] = &[
    "cmd",
    "enabled",
    "initializationOptions",
    "languages",
    "onTypeFormattingTriggers",
    "preferSharedInstance",
    "rootMarkers",
    "settings",
    "workspaceMarkers",
];

pub(crate) const KNOWN_CAPTURE_MAPPINGS_SETTING_KEYS: &[&str] = &["folds", "highlights"];

pub(crate) const KNOWN_LANGUAGE_SETTING_KEYS: &[&str] =
    &["aliases", "base", "bridge", "layers", "parser", "queries"];

pub(crate) const KNOWN_LAYER_AGGREGATION_SETTING_KEYS: &[&str] = &["priorities", "strategy"];

pub(crate) const KNOWN_LAYERS_SETTING_KEYS: &[&str] = &["aggregation"];

pub(crate) const KNOWN_QUERY_ITEM_SETTING_KEYS: &[&str] = &["kind", "path"];

pub(crate) fn sort_and_dedup_unknown_keys(unknown_keys: &mut Vec<String>) {
    unknown_keys.sort();
    unknown_keys.dedup();
}

fn unknown_object_keys(path: &str, value: &Value, known_keys: &[&str]) -> Vec<String> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };

    object
        .keys()
        .filter(|key| !known_keys.contains(&key.as_str()))
        .map(|key| format!("{path}.{key}"))
        .collect()
}

pub(crate) fn is_workspace_setting_key_or_typo(key: &str) -> bool {
    KNOWN_WORKSPACE_SETTING_KEYS.contains(&key)
        || KNOWN_WORKSPACE_SETTING_KEYS
            .iter()
            .any(|known_key| is_one_edit_apart(key, known_key))
}

fn is_one_edit_apart(candidate: &str, known: &str) -> bool {
    let candidate = candidate.as_bytes();
    let known = known.as_bytes();
    let len_diff = candidate.len().abs_diff(known.len());
    if len_diff > 1 {
        return false;
    }

    if len_diff == 0 {
        return candidate
            .iter()
            .zip(known.iter())
            .filter(|(candidate, known)| candidate != known)
            .count()
            == 1;
    }

    let (shorter, longer) = if candidate.len() < known.len() {
        (candidate, known)
    } else {
        (known, candidate)
    };
    let mut short_index = 0;
    let mut long_index = 0;
    let mut edits = 0;
    while short_index < shorter.len() && long_index < longer.len() {
        if shorter[short_index] == longer[long_index] {
            short_index += 1;
        } else {
            edits += 1;
            if edits > 1 {
                return false;
            }
        }
        long_index += 1;
    }

    true
}

pub(crate) fn unknown_workspace_setting_keys(settings: &Value) -> Vec<String> {
    let Some(object) = settings.as_object() else {
        return Vec::new();
    };

    let mut unknown_keys = object
        .keys()
        .filter(|key| !KNOWN_WORKSPACE_SETTING_KEYS.contains(&key.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    append_unknown_bridge_server_setting_keys(object, &mut unknown_keys);
    append_unknown_capture_mappings_setting_keys(object, &mut unknown_keys);
    append_unknown_language_setting_keys(object, &mut unknown_keys);

    unknown_keys
}

fn append_unknown_bridge_server_setting_keys(
    object: &serde_json::Map<String, Value>,
    unknown_keys: &mut Vec<String>,
) {
    let Some(servers) = object.get("languageServers").and_then(Value::as_object) else {
        return;
    };

    for (server_name, server) in servers {
        unknown_keys.extend(unknown_object_keys(
            &format!("languageServers.{server_name}"),
            server,
            KNOWN_BRIDGE_SERVER_SETTING_KEYS,
        ));
    }
}

fn append_unknown_capture_mappings_setting_keys(
    object: &serde_json::Map<String, Value>,
    unknown_keys: &mut Vec<String>,
) {
    let Some(capture_mappings) = object.get("captureMappings").and_then(Value::as_object) else {
        return;
    };

    for (scope, query_type_mappings) in capture_mappings {
        unknown_keys.extend(unknown_object_keys(
            &format!("captureMappings.{scope}"),
            query_type_mappings,
            KNOWN_CAPTURE_MAPPINGS_SETTING_KEYS,
        ));
    }
}

fn append_unknown_language_setting_keys(
    object: &serde_json::Map<String, Value>,
    unknown_keys: &mut Vec<String>,
) {
    let Some(languages) = object.get("languages").and_then(Value::as_object) else {
        return;
    };

    for (language_name, language) in languages {
        let language_path = format!("languages.{language_name}");
        unknown_keys.extend(unknown_object_keys(
            &language_path,
            language,
            KNOWN_LANGUAGE_SETTING_KEYS,
        ));

        let Some(language) = language.as_object() else {
            continue;
        };

        append_unknown_query_item_keys(&language_path, language, unknown_keys);
        append_unknown_bridge_language_keys(&language_path, language, unknown_keys);
        append_unknown_layers_keys(&language_path, language, unknown_keys);
    }
}

fn append_unknown_query_item_keys(
    language_path: &str,
    language: &serde_json::Map<String, Value>,
    unknown_keys: &mut Vec<String>,
) {
    let Some(queries) = language.get("queries").and_then(Value::as_array) else {
        return;
    };

    for (query_index, query) in queries.iter().enumerate() {
        unknown_keys.extend(unknown_object_keys(
            &format!("{language_path}.queries.{query_index}"),
            query,
            KNOWN_QUERY_ITEM_SETTING_KEYS,
        ));
    }
}

fn append_unknown_bridge_language_keys(
    language_path: &str,
    language: &serde_json::Map<String, Value>,
    unknown_keys: &mut Vec<String>,
) {
    let Some(bridge) = language.get("bridge").and_then(Value::as_object) else {
        return;
    };

    for (bridge_name, bridge) in bridge {
        let bridge_path = format!("{language_path}.bridge.{bridge_name}");
        unknown_keys.extend(unknown_object_keys(
            &bridge_path,
            bridge,
            KNOWN_BRIDGE_LANGUAGE_SETTING_KEYS,
        ));

        let Some(bridge) = bridge.as_object() else {
            continue;
        };
        let Some(aggregation) = bridge.get("aggregation").and_then(Value::as_object) else {
            continue;
        };

        for (method, config) in aggregation {
            unknown_keys.extend(unknown_object_keys(
                &format!("{bridge_path}.aggregation.{method}"),
                config,
                KNOWN_AGGREGATION_SETTING_KEYS,
            ));
        }
    }
}

fn append_unknown_layers_keys(
    language_path: &str,
    language: &serde_json::Map<String, Value>,
    unknown_keys: &mut Vec<String>,
) {
    let Some(layers) = language.get("layers") else {
        return;
    };

    let layers_path = format!("{language_path}.layers");
    unknown_keys.extend(unknown_object_keys(
        &layers_path,
        layers,
        KNOWN_LAYERS_SETTING_KEYS,
    ));

    let Some(aggregation) = layers.get("aggregation").and_then(Value::as_object) else {
        return;
    };

    for (method, config) in aggregation {
        unknown_keys.extend(unknown_object_keys(
            &format!("{layers_path}.aggregation.{method}"),
            config,
            KNOWN_LAYER_AGGREGATION_SETTING_KEYS,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{
        AggregationConfig, BridgeLanguageConfig, BridgeServerConfig, FeatureSettings,
        LanguageSettings, LayerAggregationConfig, LayersConfig, QueryItem, QueryTypeMappings,
        RawWorkspaceSettings,
    };
    use std::collections::BTreeSet;

    fn assert_known_keys_match_schema<T: schemars::JsonSchema>(
        known_keys: &[&str],
        aliases: &[&str],
    ) {
        let schema = schemars::schema_for!(T);
        let value = serde_json::to_value(schema).expect("schema should serialize");
        let properties = value["properties"]
            .as_object()
            .expect("schema should have properties");

        let mut schema_keys = properties.keys().cloned().collect::<BTreeSet<_>>();
        schema_keys.extend(aliases.iter().map(|alias| (*alias).to_string()));
        let known_keys = known_keys
            .iter()
            .map(|key| (*key).to_string())
            .collect::<BTreeSet<_>>();

        assert_eq!(known_keys, schema_keys);
    }

    #[test]
    fn known_workspace_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<RawWorkspaceSettings>(KNOWN_WORKSPACE_SETTING_KEYS, &[]);
    }

    #[test]
    fn known_feature_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<FeatureSettings>(KNOWN_FEATURE_SETTING_KEYS, &[]);
    }

    #[test]
    fn section_sibling_filter_matches_workspace_keys_and_typos_only() {
        assert!(is_workspace_setting_key_or_typo("autoInstall"));
        assert!(is_workspace_setting_key_or_typo("autoInstal"));
        assert!(is_workspace_setting_key_or_typo("languageServers"));
        assert!(!is_workspace_setting_key_or_typo("autXInstal"));

        assert!(!is_workspace_setting_key_or_typo("editor"));
        assert!(!is_workspace_setting_key_or_typo("files"));
        assert!(!is_workspace_setting_key_or_typo("workbench"));
    }

    #[test]
    fn known_bridge_server_setting_keys_match_schema_properties_and_aliases() {
        assert_known_keys_match_schema::<BridgeServerConfig>(
            KNOWN_BRIDGE_SERVER_SETTING_KEYS,
            &["rootMarkers"],
        );
    }

    #[test]
    fn known_capture_mappings_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<QueryTypeMappings>(
            KNOWN_CAPTURE_MAPPINGS_SETTING_KEYS,
            &[],
        );
    }

    #[test]
    fn known_language_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<LanguageSettings>(KNOWN_LANGUAGE_SETTING_KEYS, &[]);
    }

    #[test]
    fn known_query_item_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<QueryItem>(KNOWN_QUERY_ITEM_SETTING_KEYS, &[]);
    }

    #[test]
    fn known_bridge_language_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<BridgeLanguageConfig>(
            KNOWN_BRIDGE_LANGUAGE_SETTING_KEYS,
            &[],
        );
    }

    #[test]
    fn known_aggregation_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<AggregationConfig>(KNOWN_AGGREGATION_SETTING_KEYS, &[]);
    }

    #[test]
    fn known_layers_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<LayersConfig>(KNOWN_LAYERS_SETTING_KEYS, &[]);
    }

    #[test]
    fn known_layer_aggregation_setting_keys_match_schema_properties() {
        assert_known_keys_match_schema::<LayerAggregationConfig>(
            KNOWN_LAYER_AGGREGATION_SETTING_KEYS,
            &[],
        );
    }
}
