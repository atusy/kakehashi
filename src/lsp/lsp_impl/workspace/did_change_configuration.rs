//! didChangeConfiguration notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidChangeConfigurationParams;

use crate::config::{RawWorkspaceSettings, WorkspaceSettings, merge_workspace_settings};

use super::super::Kakehashi;

impl Kakehashi {
    /// Handle workspace/didChangeConfiguration notification.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        let parsed = match parse_client_configuration(params.settings) {
            Ok(update) => update,
            Err(err) => {
                self.notifier()
                    .log_warning(format!("Failed to parse client configuration: {}", err))
                    .await;
                return;
            }
        };

        // Nudge users off the deprecated `rootMarkers` key if this push carries
        // it. Detect from the raw value before serde's alias erases which key
        // was used; the claim guard shares the once-per-session slot with the
        // initialize path.
        if crate::config::deprecation::json_uses_deprecated_root_markers(&parsed.raw_value)
            && self
                .settings_manager
                .claim_root_markers_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::ROOT_MARKERS_DEPRECATION_NOTICE)
                .await;
        }

        for warning in &parsed.warnings {
            self.notifier().log_warning(warning).await;
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
    raw_value: serde_json::Value,
    warnings: Vec<String>,
}

fn parse_client_configuration(
    value: serde_json::Value,
) -> Result<ClientConfigurationUpdate, serde_json::Error> {
    let raw_value = unwrap_kakehashi_settings(value);
    let warnings = ignored_key_warnings(&raw_value);
    let settings = serde_json::from_value::<RawWorkspaceSettings>(raw_value.clone())?;

    Ok(ClientConfigurationUpdate {
        settings,
        raw_value,
        warnings,
    })
}

fn unwrap_kakehashi_settings(value: serde_json::Value) -> serde_json::Value {
    value
        .get("settings")
        .and_then(|settings| settings.get("kakehashi"))
        .cloned()
        .or_else(|| value.get("kakehashi").cloned())
        .unwrap_or(value)
}

fn ignored_key_warnings(value: &serde_json::Value) -> Vec<String> {
    let Some(object) = value.as_object() else {
        return Vec::new();
    };

    let mut ignored: Vec<_> = object
        .keys()
        .filter(|key| {
            !matches!(
                key.as_str(),
                "searchPaths"
                    | "languages"
                    | "captureMappings"
                    | "autoInstall"
                    | "diagnosticsDebounceMs"
                    | "languageServers"
            )
        })
        .cloned()
        .collect();
    ignored.sort();

    if ignored.is_empty() {
        Vec::new()
    } else {
        vec![format!(
            "Ignored unknown client configuration key(s): {}",
            ignored.join(", ")
        )]
    }
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
    fn warns_about_unknown_flat_keys() {
        let update = parse_client_configuration(serde_json::json!({
            "autoInstal": false,
            "autoInstall": true
        }))
        .expect("settings with unknown keys should still parse");

        assert_eq!(update.settings.auto_install, Some(true));
        assert_eq!(
            update.warnings,
            vec!["Ignored unknown client configuration key(s): autoInstal"]
        );
    }
}
