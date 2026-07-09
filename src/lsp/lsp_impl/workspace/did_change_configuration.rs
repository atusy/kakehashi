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

        let parsed = match parse_normalized_client_configuration(normalized) {
            Ok(update) => update,
            Err(err) => {
                self.notifier()
                    .log_warning(format!("Failed to parse client configuration: {}", err))
                    .await;
                return;
            }
        };

        for warning in &parsed.warnings {
            self.notifier().log_warning(warning).await;
        }

        if raw_workspace_settings_is_empty(&parsed.settings) {
            return;
        }

        // Merge onto current effective settings (not from scratch).
        // The current settings already reflect defaults < user < project < initializationOptions,
        // so merging preserves languages and other fields set during initialize.
        let current_ts = self.settings_manager.load_raw_settings();
        // SAFETY: merge_workspace_settings(Some, Some) always returns Some, so unwrap_or_return is
        // defensive only — the None branch is unreachable under the current implementation.
        let Some(merged_ts) =
            merge_workspace_settings(Some((*current_ts).clone()), Some(parsed.settings))
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
    let settings_root = value
        .get("settings")
        .cloned()
        .unwrap_or_else(|| value.clone());
    let Some(inner) = settings_root
        .get("settings")
        .and_then(|settings| settings.get("kakehashi"))
        .filter(|value| value.is_object())
        .or_else(|| settings_root.get("kakehashi"))
        .filter(|value| value.is_object())
        .or_else(|| value.get("kakehashi"))
        .filter(|value| value.is_object())
    else {
        let warnings = if has_known_configuration_key(&settings_root) {
            ignored_key_warnings(&settings_root)
        } else {
            Vec::new()
        };
        return NormalizedClientConfiguration {
            raw_value: settings_root,
            warnings,
        };
    };

    let mut warnings = Vec::new();
    let mut raw_value = inner.clone();

    if let Some(raw_object) = raw_value.as_object_mut()
        && let Some(root_object) = value.as_object()
    {
        for (key, candidate) in root_object {
            if key == "settings" || key == "kakehashi" {
                continue;
            }

            if is_known_configuration_key(key) {
                raw_object
                    .entry(key.clone())
                    .or_insert_with(|| candidate.clone());
            } else {
                warnings.push(key.clone());
            }
        }
    }

    warnings.extend(ignored_keys(&raw_value));
    warnings.sort();
    warnings.dedup();

    if warnings.is_empty() {
        NormalizedClientConfiguration {
            raw_value,
            warnings: Vec::new(),
        }
    } else {
        NormalizedClientConfiguration {
            raw_value,
            warnings: vec![format!(
                "Ignored unknown client configuration key(s): {}",
                warnings.join(", ")
            )],
        }
    }
}

fn ignored_key_warnings(value: &serde_json::Value) -> Vec<String> {
    let ignored = ignored_keys(value);
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
        .keys()
        .filter(|key| !is_known_configuration_key(key))
        .cloned()
        .collect();
    ignored.sort();
    ignored
}

fn has_known_configuration_key(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|object| {
        object
            .keys()
            .any(|key| is_known_configuration_key(key.as_str()))
    })
}

fn is_known_configuration_key(key: &str) -> bool {
    known_configuration_keys().contains(key)
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
    settings.search_paths.is_none()
        && settings.languages.is_empty()
        && settings.capture_mappings.is_empty()
        && settings.auto_install.is_none()
        && settings.diagnostics_debounce_ms.is_none()
        && settings.language_servers.is_none()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_section_wrapped_kakehashi_settings() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": false
                }
            }
        }))
        .expect("section-wrapped settings should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn parses_top_level_wrapped_kakehashi_settings() {
        let update = parse_client_configuration(serde_json::json!({
            "kakehashi": {
                "autoInstall": false
            }
        }))
        .expect("top-level wrapped settings should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn merges_known_flat_siblings_with_wrapped_settings() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "kakehashi": {
                    "autoInstall": false
                }
            },
            "autoInstall": true,
            "diagnosticsDebounceMs": 250,
            "editorSetting": true
        }))
        .expect("mixed wrapped and flat settings should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert_eq!(update.settings.diagnostics_debounce_ms, Some(250));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): editorSetting"]
        );
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
        assert!(update.warnings.is_empty());
    }

    #[test]
    fn parses_settings_root_known_keys() {
        let update = parse_client_configuration(serde_json::json!({
            "settings": {
                "autoInstall": false
            }
        }))
        .expect("settings-root payload should parse");

        assert_eq!(update.settings.auto_install, Some(false));
        assert!(update.warnings.is_empty());
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
