//! didChangeConfiguration notification handler for Kakehashi.

use crate::config::unknown_keys::{
    KNOWN_FEATURE_SETTING_KEYS, is_workspace_setting_key_or_typo, sort_and_dedup_unknown_keys,
    unknown_workspace_setting_keys,
};
use serde_json::Value;
use tower_lsp_server::ls_types::DidChangeConfigurationParams;

use crate::config::{RawWorkspaceSettings, WorkspaceSettings, merge_workspace_settings};

use super::super::{Kakehashi, lock_settings_reload};

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
            .filter_map(|(key, value)| is_kakehashi_workspace_entry(&key, &value).then_some(key))
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
        .filter_map(|(key, mut value)| {
            if !is_kakehashi_workspace_entry(&key, &value) {
                return None;
            }
            if key == "features"
                && let Some(features) = value.as_object_mut()
            {
                features
                    .retain(|feature, _| KNOWN_FEATURE_SETTING_KEYS.contains(&feature.as_str()));
            }
            Some((key, value))
        })
        .collect();
    Value::Object(object)
}

fn format_rejected_keys(keys: &[String]) -> String {
    keys.iter()
        .map(|key| format!("`{key}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_kakehashi_workspace_entry(key: &str, value: &Value) -> bool {
    if key == "features" {
        return value.as_object().is_some_and(|features| {
            KNOWN_FEATURE_SETTING_KEYS
                .iter()
                .any(|key| features.contains_key(*key))
        });
    }
    is_workspace_setting_key_or_typo(key)
}

impl Kakehashi {
    /// Handle workspace/didChangeConfiguration notification.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        let uses_deprecated_unwrapped_shape =
            uses_deprecated_unwrapped_didchange_shape(&params.settings);
        let (settings_value, unknown_keys) = settings_payload(params.settings);

        // Nudge users off any deprecated key this push carries. Detected from
        // the raw value before parsing collapses the deprecated and canonical
        // spellings; each claim guard shares its once-per-session slot with the
        // initialize path.
        let pushed_deprecated_keys =
            crate::config::deprecation::json_deprecated_keys(&settings_value);
        if pushed_deprecated_keys.root_markers
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
        }
        if pushed_deprecated_keys.auto_install
            && self
                .settings_manager
                .claim_auto_install_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::AUTO_INSTALL_DEPRECATION_NOTICE)
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

        // Snapshot read, derivation, and publication must share the same reload
        // transaction as post-install search-path updates, or either path can
        // overwrite a concurrent update derived from a stale snapshot.
        let reload = lock_settings_reload().await;

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
                let warnings = Self::misconfigured_settings_warnings(&settings);
                self.apply_raw_settings_locked(&reload, merged_ts, settings)
                    .await;
                drop(reload);
                self.warn_on_misconfigured_settings(&warnings).await;
                self.notifier().log_info("Configuration updated!").await;
            }
            Err(errs) => {
                drop(reload);
                let event = crate::lsp::SettingsEvent::error(format!(
                    "Invalid configuration: {errs}. \
                     This configuration has been discarded; previous settings remain in effect. \
                     Please correct the invalid settings or remove them from your config.",
                ));
                self.notifier().log_settings_events(&[event]).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::task::Poll;
    use tower_lsp_server::LspService;

    #[test]
    fn settings_payload_ignores_unrelated_features_section_sibling() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": {
                "autoInstall": true
            },
            "features": {
                "someOtherClientFeature": true
            }
        }));

        assert_eq!(payload, serde_json::json!({ "autoInstall": true }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_rejects_kakehashi_features_section_sibling() {
        let (_payload, unknown_keys) = settings_payload(serde_json::json!({
            "kakehashi": {
                "autoInstall": true
            },
            "features": {
                "workspace/diagnostic/refresh": {
                    "debounceMs": 20
                }
            }
        }));

        assert_eq!(unknown_keys, ["features"]);
    }

    #[test]
    fn settings_payload_ignores_unrelated_unwrapped_features() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "autoInstall": true,
            "features": {
                "someOtherClientFeature": true
            }
        }));

        assert_eq!(payload, serde_json::json!({ "autoInstall": true }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_keeps_unwrapped_publish_diagnostics_features() {
        let features = serde_json::json!({
            "textDocument/publishDiagnostics": {
                "debounceMs": 30,
                "maxWaitMs": 300
            }
        });
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "features": features
        }));

        assert_eq!(payload, serde_json::json!({ "features": features }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_keeps_unwrapped_log_message_feature() {
        let features = serde_json::json!({
            "window/logMessage": {
                "logLevel": "off"
            }
        });
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "features": features
        }));

        assert_eq!(payload, serde_json::json!({ "features": features }));
        assert!(unknown_keys.is_empty());
    }

    #[test]
    fn settings_payload_projects_mixed_unwrapped_features() {
        let (payload, unknown_keys) = settings_payload(serde_json::json!({
            "features": {
                "textDocument/publishDiagnostics": {
                    "debounceMs": 30,
                    "maxWaitMs": 300
                },
                "someOtherClientFeature": true
            }
        }));

        assert_eq!(
            payload,
            serde_json::json!({
                "features": {
                    "textDocument/publishDiagnostics": {
                        "debounceMs": 30,
                        "maxWaitMs": 300
                    }
                }
            })
        );
        assert!(unknown_keys.is_empty());
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

    #[tokio::test]
    async fn did_change_configuration_merges_into_settings_published_while_waiting() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server.settings_manager.apply_settings_with_raw(
            RawWorkspaceSettings {
                auto_install: Some(true),
                search_paths: Some(vec!["/initial".to_string()]),
                ..Default::default()
            },
            WorkspaceSettings {
                auto_install: true,
                search_paths: vec!["/initial".to_string()],
                ..Default::default()
            },
        );

        // This payload uses the deprecated top-level `autoInstall` on purpose
        // (the merge behavior under test is about that key). Consume the notice
        // slot first: the warning would otherwise `show_warning` into a socket
        // this test never drains, parking the future for a reason unrelated to
        // the settings-reload lock it is meant to park on.
        server
            .settings_manager
            .claim_auto_install_deprecation_warning();

        let reload_guard = crate::lsp::lsp_impl::lock_settings_reload().await;
        let mut update = Box::pin(server.did_change_configuration_impl(
            DidChangeConfigurationParams {
                settings: serde_json::json!({
                    "kakehashi": { "autoInstall": false }
                }),
            },
        ));
        std::future::poll_fn(|cx| {
            assert!(update.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        server.settings_manager.apply_settings_with_raw(
            RawWorkspaceSettings {
                auto_install: Some(true),
                search_paths: Some(vec!["/newer".to_string()]),
                ..Default::default()
            },
            WorkspaceSettings {
                auto_install: true,
                search_paths: vec!["/newer".to_string()],
                ..Default::default()
            },
        );
        drop(reload_guard);
        update.await;

        let snapshot = server.settings_manager.load_settings_pair();
        assert_eq!(snapshot.raw_settings.auto_install, Some(false));
        assert_eq!(
            snapshot.raw_settings.search_paths,
            Some(vec!["/newer".to_string()])
        );
        assert!(!snapshot.settings.auto_install);
        assert_eq!(snapshot.settings.search_paths, vec!["/newer".to_string()]);
    }
}
