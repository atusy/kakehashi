//! didChangeConfiguration notification handler for Kakehashi.

use serde_json::Value;
use tower_lsp_server::ls_types::DidChangeConfigurationParams;

use crate::config::{RawWorkspaceSettings, WorkspaceSettings, merge_workspace_settings};

use super::super::Kakehashi;

const KNOWN_WORKSPACE_SETTING_KEYS: &[&str] = &[
    "searchPaths",
    "languages",
    "captureMappings",
    "autoInstall",
    "diagnosticsDebounceMs",
    "languageServers",
];

const KNOWN_AGGREGATION_SETTING_KEYS: &[&str] = &[
    "maxFanOut",
    "priorities",
    "pullFallback",
    "pushFallback",
    "strategy",
];

const KNOWN_BRIDGE_LANGUAGE_SETTING_KEYS: &[&str] = &["aggregation", "enabled"];

const KNOWN_BRIDGE_SERVER_SETTING_KEYS: &[&str] = &[
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

const KNOWN_CAPTURE_MAPPINGS_SETTING_KEYS: &[&str] = &["folds", "highlights"];

const KNOWN_LANGUAGE_SETTING_KEYS: &[&str] =
    &["aliases", "base", "bridge", "layers", "parser", "queries"];

const KNOWN_LAYER_AGGREGATION_SETTING_KEYS: &[&str] = &["priorities", "strategy"];

const KNOWN_LAYERS_SETTING_KEYS: &[&str] = &["aggregation"];

const KNOWN_QUERY_ITEM_SETTING_KEYS: &[&str] = &["kind", "path"];

fn settings_payload(settings: Value) -> (Value, Vec<String>) {
    let Value::Object(mut object) = settings else {
        return (settings, Vec::new());
    };

    if object.contains_key("kakehashi") {
        let kakehashi = object
            .remove("kakehashi")
            .expect("kakehashi key should exist after object lookup");
        if !kakehashi.is_object() {
            return (kakehashi, Vec::new());
        }

        let mut unknown_keys = object
            .into_iter()
            .map(|(key, _)| key)
            .filter(|key| is_workspace_setting_key_or_typo(key))
            .collect::<Vec<_>>();
        unknown_keys.extend(unknown_workspace_setting_keys(&kakehashi));
        sort_and_dedup_unknown_keys(&mut unknown_keys);
        return (kakehashi, unknown_keys);
    }

    let settings = kakehashi_targeted_payload(object);
    let mut unknown_keys = unknown_workspace_setting_keys(&settings);
    sort_and_dedup_unknown_keys(&mut unknown_keys);
    (settings, unknown_keys)
}

fn sort_and_dedup_unknown_keys(unknown_keys: &mut Vec<String>) {
    unknown_keys.sort();
    unknown_keys.dedup();
}

fn uses_deprecated_unwrapped_didchange_shape(settings: &Value) -> bool {
    let Some(object) = settings.as_object() else {
        return false;
    };
    if object.contains_key("kakehashi") {
        return false;
    }

    kakehashi_targeted_payload(object.clone())
        .as_object()
        .is_some_and(|object| !object.is_empty())
}

fn kakehashi_targeted_payload(object: serde_json::Map<String, Value>) -> serde_json::Value {
    let object = object
        .into_iter()
        .filter(|(key, _)| is_workspace_setting_key_or_typo(key))
        .collect();
    Value::Object(object)
}

fn format_rejected_keys(keys: &[String]) -> String {
    keys.iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ")
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

fn is_workspace_setting_key_or_typo(key: &str) -> bool {
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

fn unknown_workspace_setting_keys(settings: &Value) -> Vec<String> {
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

impl Kakehashi {
    /// Handle workspace/didChangeConfiguration notification.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        let uses_deprecated_unwrapped_shape =
            uses_deprecated_unwrapped_didchange_shape(&params.settings);
        let (settings_value, unknown_keys) = settings_payload(params.settings);

        // Nudge users off the deprecated `rootMarkers` key if this push carries
        // it. Detect from the raw value before serde's alias erases which key
        // was used; the claim guard shares the once-per-session slot with the
        // initialize path.
        if crate::config::deprecation::json_uses_deprecated_root_markers(&settings_value)
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
        }

        if uses_deprecated_unwrapped_shape
            && self
                .settings_manager
                .claim_unwrapped_didchange_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::UNWRAPPED_DIDCHANGE_CONFIGURATION_NOTICE)
                .await;
        }

        if !unknown_keys.is_empty() {
            self.notifier()
                .log_warning(format!(
                    "workspace/didChangeConfiguration rejected configuration update containing unknown or mixed-format key(s): {}",
                    format_rejected_keys(&unknown_keys)
                ))
                .await;
            return;
        }

        if settings_value
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            return;
        }

        // Parse the incoming settings.
        let parsed = match serde_json::from_value::<RawWorkspaceSettings>(settings_value) {
            Ok(settings) => settings,
            Err(err) => {
                self.notifier()
                    .log_warning(format!("Failed to parse client configuration: {}", err))
                    .await;
                return;
            }
        };

        // Serialize the snapshot read and publication as one transaction. The
        // reload lock inside apply_shared_settings starts too late to prevent
        // two callers deriving replacements from the same old snapshot.
        let _settings_transaction = self.settings_manager.begin_settings_transaction().await;

        // Merge onto current effective settings (not from scratch).
        // The current settings already reflect defaults < user < project < initializationOptions,
        // so merging preserves languages and other fields set during initialize.
        let current_ts = self.settings_manager.load_raw_settings();
        // SAFETY: merge_workspace_settings(Some, Some) always returns Some, so unwrap_or_return is
        // defensive only — the None branch is unreachable under the current implementation.
        let Some(merged_ts) = merge_workspace_settings(Some((*current_ts).clone()), Some(parsed))
        else {
            log::warn!(
                "merge_workspace_settings returned None despite two Some inputs; skipping configuration update"
            );
            return;
        };

        match WorkspaceSettings::try_from_settings(
            &merged_ts,
            self.home_dir.as_deref(),
            crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
        ) {
            Ok(settings) => {
                self.apply_raw_settings(merged_ts, settings).await;
                drop(_settings_transaction);
                self.warn_on_misconfigured_settings().await;
                self.notifier().log_info("Configuration updated!").await;
            }
            Err(errs) => {
                drop(_settings_transaction);
                let event = crate::lsp::SettingsEvent::error(format!(
                    "Path expansion failed: {errs}. \
                     This configuration has been discarded; previous settings remain in effect. \
                     Please correct the affected paths and environment variables or remove them from your config.",
                ));
                self.notifier().log_settings_events(&[event]).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{
        AggregationConfig, BridgeLanguageConfig, BridgeServerConfig, LanguageSettings,
        LayerAggregationConfig, LayersConfig, QueryItem, QueryTypeMappings,
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
    fn non_object_kakehashi_wrapper_reaches_parse_error_path() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": null,
            "autoInstall": false
        }));

        assert_eq!(payload, serde_json::Value::Null);
        assert!(unknown_keys.is_empty());
        assert!(serde_json::from_value::<RawWorkspaceSettings>(payload).is_err());
    }

    #[test]
    fn settings_payload_deduplicates_unknown_keys() {
        let (_payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": {
                "autoInstal": true
            },
            "autoInstal": false
        }));

        assert_eq!(unknown_keys, ["autoInstal"]);
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
