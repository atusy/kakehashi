//! Rebuilding the effective settings from their retained layers.

use crate::config::{RawWorkspaceSettings, WorkspaceSettings};
use crate::lsp::load_settings_with_client_layers;

use super::super::Kakehashi;

impl Kakehashi {
    /// Fold every layer again — defaults, the configuration files, and the
    /// client's `layers` in order — anchored to `root_path`, and validate the
    /// result. Reports the fold's own events and notices; publishing is the
    /// caller's, under the settings-reload transaction it already holds.
    ///
    /// The implicit user and project files are re-read, since a rebuilt fold
    /// must see what they say now. Explicit `--config-file` layers are not:
    /// they are read exactly once, and initialize retained their parsed,
    /// already-anchored layers for this replay.
    pub(super) async fn recompose_settings(
        &self,
        root_path: Option<&std::path::Path>,
        layers: Vec<RawWorkspaceSettings>,
    ) -> Result<(RawWorkspaceSettings, WorkspaceSettings), String> {
        let outcome = load_settings_with_client_layers(
            root_path,
            layers,
            self.home_dir.as_deref(),
            |var| std::env::var(var).ok(),
            self.explicit_config.get().cloned().flatten(),
        );
        self.notifier().log_settings_events(&outcome.events).await;
        if outcome.deprecated_keys.capture_mappings
            && self
                .settings_manager
                .claim_capture_mappings_deprecation_warning()
        {
            self.notifier()
                .show_warning(crate::config::deprecation::CAPTURE_MAPPINGS_DEPRECATION_NOTICE)
                .await;
        }
        if let Some(notice) = outcome.empty_container_notice.as_deref()
            && self
                .settings_manager
                .claim_empty_container_migration_warning()
        {
            self.notifier().show_warning(notice).await;
        }
        let raw = outcome
            .raw_settings
            .unwrap_or_else(crate::config::defaults::default_settings);
        WorkspaceSettings::try_from_settings(
            &raw,
            self.home_dir.as_deref(),
            crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
        )
        .map(|settings| (raw, settings))
        .map_err(|error| error.to_string())
    }
}
