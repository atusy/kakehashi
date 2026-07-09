//! didChangeConfiguration notification handler for Kakehashi.

use std::collections::HashSet;
use std::sync::OnceLock;

use tower_lsp_server::ls_types::DidChangeConfigurationParams;

use crate::config::{RawWorkspaceSettings, WorkspaceSettings, merge_workspace_settings};

use super::super::Kakehashi;

impl Kakehashi {
    /// Handle workspace/didChangeConfiguration notification.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        let normalized = normalize_kakehashi_settings(params.settings);

        // Nudge users off the deprecated `rootMarkers` key if this push carries
        // it. Detect from the raw value before serde's alias erases which key
        // was used; the claim guard shares the once-per-session slot with the
        // initialize path.
        if crate::config::deprecation::json_uses_deprecated_root_markers(&normalized.raw_value)
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
        }

        if normalized.uses_deprecated_unwrapped_shape
            && self
                .settings_manager
                .claim_unwrapped_didchange_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::UNWRAPPED_DIDCHANGE_CONFIGURATION_NOTICE)
                .await;
        }

        let parsed = match parse_normalized_client_configuration(normalized) {
            Ok(update) => update,
            Err(err) => {
                self.notifier()
                    .log_warning(format!("Failed to parse client configuration: {}", err))
                    .await;
                return;
            }
        };

        let ClientConfigurationUpdate { settings, warnings } = parsed;

        for warning in warnings {
            self.notifier().log_warning(warning).await;
        }

        if raw_workspace_settings_is_empty(&settings) {
            return;
        }

        // Merge onto current effective settings (not from scratch).
        // The current settings already reflect defaults < user < project < initializationOptions,
        // so merging preserves languages and other fields set during initialize.
        let current_ts = self.settings_manager.load_raw_settings();
        // SAFETY: merge_workspace_settings(Some, Some) always returns Some, so unwrap_or_return is
        // defensive only — the None branch is unreachable under the current implementation.
        let Some(merged_ts) = merge_workspace_settings(Some((*current_ts).clone()), Some(settings))
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
                self.notifier().log_info("Configuration updated!").await;
            }
            Err(errs) => {
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

struct ClientConfigurationUpdate {
    settings: RawWorkspaceSettings,
    warnings: Vec<String>,
}

struct NormalizedClientConfiguration {
    raw_value: serde_json::Value,
    warnings: Vec<String>,
    uses_deprecated_unwrapped_shape: bool,
}

#[cfg(test)]
fn parse_client_configuration(
    value: serde_json::Value,
) -> Result<ClientConfigurationUpdate, serde_json::Error> {
    parse_normalized_client_configuration(normalize_kakehashi_settings(value))
}

fn parse_normalized_client_configuration(
    normalized: NormalizedClientConfiguration,
) -> Result<ClientConfigurationUpdate, serde_json::Error> {
    let settings = serde_json::from_value::<RawWorkspaceSettings>(normalized.raw_value)?;

    Ok(ClientConfigurationUpdate {
        settings,
        warnings: normalized.warnings,
    })
}

fn normalize_kakehashi_settings(value: serde_json::Value) -> NormalizedClientConfiguration {
    let Some(inner) = value.get("kakehashi").filter(|value| value.is_object()) else {
        let top_level_flat = flat_configuration_without_wrappers(&value);
        let top_level_flat_has_signal = has_configuration_signal(&top_level_flat);
        let (raw_value, has_signal) = if top_level_flat_has_signal {
            (top_level_flat, true)
        } else {
            (serde_json::Value::Object(serde_json::Map::new()), false)
        };
        let warnings = if has_signal {
            ignored_key_warnings_from_keys(ignored_keys(&raw_value))
        } else {
            Vec::new()
        };
        return NormalizedClientConfiguration {
            raw_value,
            warnings,
            uses_deprecated_unwrapped_shape: has_signal,
        };
    };

    let mut warnings = Vec::new();
    let mut raw_value = inner.clone();
    let mut uses_deprecated_unwrapped_shape = false;

    if let Some(raw_object) = raw_value.as_object_mut()
        && let Some(root_object) = value.as_object()
    {
        uses_deprecated_unwrapped_shape |=
            merge_flat_sibling_settings(raw_object, &mut warnings, root_object);
    }

    warnings.extend(ignored_keys(&raw_value));
    warnings.sort();
    warnings.dedup();

    if warnings.is_empty() {
        NormalizedClientConfiguration {
            raw_value,
            warnings: Vec::new(),
            uses_deprecated_unwrapped_shape,
        }
    } else {
        NormalizedClientConfiguration {
            raw_value,
            warnings: vec![format!(
                "Ignored unknown client configuration key(s): {}",
                warnings.join(", ")
            )],
            uses_deprecated_unwrapped_shape,
        }
    }
}

fn ignored_key_warnings_from_keys(mut ignored: Vec<String>) -> Vec<String> {
    ignored.sort();
    ignored.dedup();
    if ignored.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Ignored unknown client configuration key(s): {}",
            ignored.join(", ")
        )]
    }
}

fn ignored_keys(value: &serde_json::Value) -> Vec<String> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };

    let mut ignored: Vec<_> = object
        .iter()
        .filter(|(key, value)| should_warn_unknown_key(key, value))
        .map(|(key, _)| key.clone())
        .collect();
    ignored.sort();
    ignored
}

fn flat_configuration_without_wrappers(value: &serde_json::Value) -> serde_json::Value {
    let Some(object) = value.as_object() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };

    serde_json::Value::Object(
        object
            .iter()
            .filter(|(key, _)| key.as_str() != "settings" && key.as_str() != "kakehashi")
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    )
}

fn merge_flat_sibling_settings(
    raw_object: &mut serde_json::Map<String, serde_json::Value>,
    warnings: &mut Vec<String>,
    siblings: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    let mut has_flat_configuration_signal = false;

    for (key, candidate) in siblings {
        if key == "settings" || key == "kakehashi" {
            continue;
        }

        if is_known_configuration_key(key) {
            has_flat_configuration_signal = true;
            raw_object
                .entry(key.clone())
                .or_insert_with(|| candidate.clone());
        } else if should_warn_unknown_key(key, candidate) {
            has_flat_configuration_signal = true;
            warnings.push(key.clone());
        }
    }

    has_flat_configuration_signal
}

fn has_configuration_signal(value: &serde_json::Value) -> bool {
    has_known_configuration_key(value) || has_probable_unknown_configuration_key(value)
}

fn has_known_configuration_key(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .keys()
            .any(|key| is_known_configuration_key(key.as_str()))
    })
}

fn has_probable_unknown_configuration_key(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .iter()
            .any(|(key, value)| should_warn_unknown_key(key, value))
    })
}

fn should_warn_unknown_key(key: &str, value: &serde_json::Value) -> bool {
    !is_known_configuration_key(key)
        && key != "settings"
        && key != "kakehashi"
        && !key.contains('.')
        && if value.is_object() {
            is_one_edit_from_known_configuration_key(key)
        } else {
            key.chars().any(|ch| ch.is_ascii_uppercase())
                || is_one_edit_from_known_configuration_key(key)
        }
}

fn is_known_configuration_key(key: &str) -> bool {
    known_configuration_keys().contains(key)
}

fn is_one_edit_from_known_configuration_key(key: &str) -> bool {
    known_configuration_keys()
        .iter()
        .any(|known| is_one_edit_apart(key, known))
}

fn is_one_edit_apart(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let len_diff = left.len().abs_diff(right.len());
    if len_diff > 1 {
        return false;
    }

    if len_diff == 0 {
        return left
            .iter()
            .zip(right.iter())
            .filter(|(left, right)| left != right)
            .count()
            == 1;
    }

    let (shorter, longer) = if left.len() < right.len() {
        (left, right)
    } else {
        (right, left)
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

fn known_configuration_keys() -> &'static HashSet<String> {
    static KEYS: OnceLock<HashSet<String>> = OnceLock::new();

    KEYS.get_or_init(|| {
        let schema = serde_json::to_value(crate::config::json_schema()).unwrap_or_else(|err| {
            log::warn!("failed to serialize RawWorkspaceSettings schema: {err}");
            serde_json::Value::Null
        });

        schema
            .get("properties")
            .and_then(|properties| properties.as_object())
            .map(|properties| properties.keys().cloned().collect())
            .unwrap_or_default()
    })
}

fn raw_workspace_settings_is_empty(settings: &RawWorkspaceSettings) -> bool {
    settings.is_effectively_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wire_wrapped_kakehashi_settings() {
        let params = serde_json::from_value::<DidChangeConfigurationParams>(serde_json::json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": false
                }
            }
        }))
        .expect("wire didChangeConfiguration params should deserialize");
        let update = parse_client_configuration(params.settings)
            .expect("wire settings.kakehashi payload should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn merges_known_flat_siblings_with_wrapped_settings() {
        let payload = serde_json::json!({
            "kakehashi": {
                "autoInstall": false
            },
            "autoInstall": true,
            "diagnosticsDebounceMs": 250,
            "editorSetting": true
        });
        let update = parse_client_configuration(payload.clone())
            .expect("mixed wrapped and flat settings should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert_eq!(update.settings.diagnostics_debounce_ms, Some(250));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): editorSetting"]
        );
        assert!(
            normalize_kakehashi_settings(payload).uses_deprecated_unwrapped_shape,
            "accepted flat siblings beside wrapped settings should still warn as deprecated"
        );

        let normalized = normalize_kakehashi_settings(serde_json::json!({
            "kakehashi": {
                "autoInstall": false
            }
        }));
        assert!(
            !normalized.uses_deprecated_unwrapped_shape,
            "canonical wire settings.kakehashi payloads must not warn as deprecated"
        );
    }

    #[test]
    fn ignores_nested_settings_flat_siblings_with_wrapped_settings() {
        let payload = serde_json::json!({
            "kakehashi": {
                "autoInstall": false
            },
            "settings": {
                "diagnosticsDebounceMs": 250,
                "editorSetting": true
            }
        });
        let update = parse_client_configuration(payload.clone())
            .expect("unsupported nested settings beside wrapped settings should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert_eq!(update.settings.diagnostics_debounce_ms, None);
        assert!(update.warnings.is_empty());
        assert!(
            !normalize_kakehashi_settings(payload).uses_deprecated_unwrapped_shape,
            "unsupported nested settings are ignored rather than treated as accepted flat siblings"
        );
    }

    #[test]
    fn flat_siblings_do_not_override_wrapped_settings() {
        let payload = serde_json::json!({
            "kakehashi": {
                "autoInstall": false
            },
            "autoInstall": true
        });
        let update = parse_client_configuration(payload.clone())
            .expect("conflicting mixed settings should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
        assert!(
            normalize_kakehashi_settings(payload).uses_deprecated_unwrapped_shape,
            "accepted conflicting flat siblings beside wrapped settings should warn as deprecated"
        );
    }

    #[test]
    fn ignores_double_wrapped_kakehashi_settings() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": false
                },
                "diagnosticsDebounceMs": 250
            }
        }))
        .expect("unsupported double-wrapped settings should parse as an empty update");

        assert!(raw_workspace_settings_is_empty(&update.settings));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn warns_about_unknown_wrapped_keys() {
        let update = parse_client_configuration(serde_json::json!({
            "kakehashi": {
                "autoInstal": false,
                "autoInstall": true
            }
        }))
        .expect("settings with unknown keys should still parse");

        assert_eq!(update.settings.auto_install, Some(true));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): autoInstal"]
        );
    }

    #[test]
    fn recognizes_empty_client_configuration() {
        let update = parse_client_configuration(serde_json::json!({
            "autoInstal": false
        }))
        .expect("unknown-only settings should still parse");

        assert!(raw_workspace_settings_is_empty(&update.settings));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): autoInstal"]
        );

        let normalized = normalize_kakehashi_settings(serde_json::json!({
            "autoInstal": false
        }));
        assert!(
            normalized.uses_deprecated_unwrapped_shape,
            "typo-like flat didChange payloads should still surface the flat-shape deprecation"
        );
    }

    #[test]
    fn parses_flat_known_keys() {
        let payload = serde_json::json!({
            "autoInstall": false
        });
        let update =
            parse_client_configuration(payload.clone()).expect("flat payload should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());

        let normalized = normalize_kakehashi_settings(payload);
        assert!(
            normalized.uses_deprecated_unwrapped_shape,
            "flat didChange payloads should be accepted but deprecated"
        );
    }

    #[test]
    fn warns_about_unknown_flat_keys() {
        let update = parse_client_configuration(serde_json::json!({
            "autoInstall": false,
            "autoInstal": true
        }))
        .expect("flat payload with typo should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): autoInstal"]
        );
    }

    #[test]
    fn parses_multiple_flat_known_keys() {
        let update = parse_client_configuration(serde_json::json!({
            "autoInstall": false,
            "diagnosticsDebounceMs": 250
        }))
        .expect("flat payload with multiple known keys should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert_eq!(update.settings.diagnostics_debounce_ms, Some(250));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn parses_top_level_known_keys_when_settings_contains_other_servers() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "gopls": {
                    "usePlaceholders": true
                }
            },
            "autoInstall": false
        }))
        .expect("top-level known settings should parse beside other server settings");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn unrelated_settings_do_not_trigger_flat_shape_deprecation() {
        let normalized = normalize_kakehashi_settings(serde_json::json!({
            "settings": {
                "gopls": {
                    "usePlaceholders": true
                }
            }
        }));

        assert!(
            !normalized.uses_deprecated_unwrapped_shape,
            "unrelated editor/server settings must not claim the flat-shape deprecation slot"
        );
        assert!(raw_workspace_settings_is_empty(
            &parse_normalized_client_configuration(normalized)
                .expect("unrelated settings should parse as an empty update")
                .settings
        ));
    }

    #[test]
    fn ignores_other_servers_settings_without_warning() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "gopls": {
                    "usePlaceholders": true
                }
            }
        }))
        .expect("other servers' settings should parse as an empty update");

        assert!(raw_workspace_settings_is_empty(&update.settings));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn ignores_dotted_editor_settings_without_warning() {
        let update = parse_client_configuration(serde_json::json!({
            "editor.fontSize": 14,
            "gopls": {
                "usePlaceholders": true
            }
        }))
        .expect("editor settings should parse as an empty update");

        assert!(raw_workspace_settings_is_empty(&update.settings));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn warns_about_object_valued_camel_case_typos() {
        let update = parse_client_configuration(serde_json::json!({
            "languageServerss": {},
            "autoInstall": true
        }))
        .expect("object-valued config typo should still parse");

        assert_eq!(update.settings.auto_install, Some(true));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): languageServerss"]
        );
    }

    #[test]
    fn ignores_unrelated_object_sections_beside_wrapped_settings() {
        let normalized = normalize_kakehashi_settings(serde_json::json!({
            "kakehashi": {
                "autoInstall": false
            },
            "someServer": {
                "usePlaceholders": true
            }
        }));

        assert!(
            !normalized.uses_deprecated_unwrapped_shape,
            "unrelated object sections must not claim the flat-shape deprecation slot"
        );
        let update = parse_normalized_client_configuration(normalized)
            .expect("wrapped settings with unrelated object section should parse");
        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn warns_about_lowercase_object_typos_near_known_keys() {
        let update = parse_client_configuration(serde_json::json!({
            "languagess": {}
        }))
        .expect("lowercase object-valued config typo should still parse");

        assert!(raw_workspace_settings_is_empty(&update.settings));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): languagess"]
        );
    }

    #[test]
    fn non_object_kakehashi_settings_are_empty() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "kakehashi": null
            }
        }))
        .expect("non-object wrapped settings should parse as an empty update");

        assert!(raw_workspace_settings_is_empty(&update.settings));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn known_configuration_keys_come_from_schema_properties() {
        let schema = serde_json::to_value(crate::config::json_schema())
            .expect("RawWorkspaceSettings schema should serialize");
        let expected: HashSet<_> = schema
            .get("properties")
            .and_then(|properties| properties.as_object())
            .expect("schema should define top-level properties")
            .keys()
            .cloned()
            .collect();

        assert_eq!(known_configuration_keys(), &expected);
        assert!(is_known_configuration_key("autoInstall"));
        assert!(!is_known_configuration_key("autoInstal"));
    }

    #[test]
    fn language_servers_empty_map_is_effective_configuration() {
        let update = parse_client_configuration(serde_json::json!({
            "languageServers": {}
        }))
        .expect("empty languageServers should parse");

        assert!(!raw_workspace_settings_is_empty(&update.settings));
    }

    #[test]
    fn normalization_preserves_deprecated_root_markers_before_parse_error() {
        let normalized = normalize_kakehashi_settings(serde_json::json!({
            "languageServers": {
                "rust-analyzer": {
                    "rootMarkers": [".git"],
                    "cmd": "rust-analyzer"
                }
            }
        }));

        assert!(
            crate::config::deprecation::json_uses_deprecated_root_markers(&normalized.raw_value)
        );
        assert!(parse_normalized_client_configuration(normalized).is_err());
    }
}
