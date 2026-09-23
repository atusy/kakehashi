//! Rebuilding the effective settings from their retained layers.

use crate::config::{RawWorkspaceSettings, WorkspaceSettings};
use crate::error::LockResultExt;
use crate::lsp::settings::SettingsLoadOutcome;
use crate::lsp::{load_settings_over_base, load_settings_with_client_layers};

use super::super::Kakehashi;

/// Settings rebuilt from their layers, ready to publish.
pub(super) struct Recomposed {
    pub(super) raw: RawWorkspaceSettings,
    pub(super) settings: WorkspaceSettings,
    /// The defaults-and-files prefix the rebuild folded the client layers
    /// over, to be retained alongside the published settings.
    pub(super) base: RawWorkspaceSettings,
}

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
    ) -> Result<Recomposed, String> {
        let outcome = load_settings_with_client_layers(
            root_path,
            layers,
            self.home_dir.as_deref(),
            |var| std::env::var(var).ok(),
            self.explicit_config.get().cloned().flatten(),
        );
        self.finish_recomposition(outcome).await
    }

    /// Fold the client's `layers` over the defaults-and-files prefix retained
    /// from the last load, without reading any file — for when only a client
    /// layer changed. The files keep the reading they had when the root was
    /// selected, so a file saved half-edited since cannot drop out of effect
    /// because the editor's settings changed.
    pub(super) async fn recompose_client_layers(
        &self,
        root_path: Option<&std::path::Path>,
        layers: Vec<RawWorkspaceSettings>,
    ) -> Result<Recomposed, String> {
        let base = self
            .settings_base
            .read()
            .recover_poison("settings_base recompose")
            .clone();
        let outcome =
            load_settings_over_base(base, layers, root_path, self.home_dir.as_deref(), |var| {
                std::env::var(var).ok()
            });
        self.finish_recomposition(outcome).await
    }

    /// Report a rebuild's events and notices, and validate its result.
    async fn finish_recomposition(
        &self,
        outcome: SettingsLoadOutcome,
    ) -> Result<Recomposed, String> {
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
        let base = outcome
            .base
            .unwrap_or_else(crate::config::defaults::default_settings);
        WorkspaceSettings::try_from_settings(
            &raw,
            self.home_dir.as_deref(),
            crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
        )
        .map(|settings| Recomposed {
            raw,
            settings,
            base,
        })
        .map_err(|error| error.to_string())
    }
}
