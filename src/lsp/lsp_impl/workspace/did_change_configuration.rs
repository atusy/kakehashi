//! didChangeConfiguration notification handler for Kakehashi.

use tower_lsp_server::ls_types::DidChangeConfigurationParams;

use crate::config::{RawWorkspaceSettings, WorkspaceSettings, merge_workspace_settings};

use super::super::Kakehashi;

impl Kakehashi {
    /// Handle workspace/didChangeConfiguration notification.
    pub(crate) async fn did_change_configuration_impl(&self, params: DidChangeConfigurationParams) {
        // Parse the incoming settings
        let parsed = match serde_json::from_value::<RawWorkspaceSettings>(params.settings) {
            Ok(settings) => settings,
            Err(err) => {
                self.notifier()
                    .log_warning(format!("Failed to parse client configuration: {}", err))
                    .await;
                return;
            }
        };

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
