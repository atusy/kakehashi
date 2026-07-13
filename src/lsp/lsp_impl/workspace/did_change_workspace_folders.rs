//! Upstream `workspace/didChangeWorkspaceFolders` lifecycle handling.

use tower_lsp_server::ls_types::DidChangeWorkspaceFoldersParams;

use crate::config::WorkspaceSettings;
use crate::lsp::{SettingsSource, load_settings};

use super::super::Kakehashi;

impl Kakehashi {
    pub(crate) async fn did_change_workspace_folders_impl(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        let added = params.event.added;
        let removed = params.event.removed;
        self.bridge
            .pool()
            .apply_workspace_folder_change(added, &removed)
            .await;
        let root_path = self
            .bridge
            .pool()
            .workspace_folders()
            .and_then(|folders| folders.first().cloned())
            .and_then(|folder| super::super::uri_to_url(&folder.uri).ok())
            .and_then(|url| url.to_file_path().ok());
        self.settings_manager.set_root_path(root_path);

        let root_path = self.settings_manager.root_path().as_ref().clone();
        let outcome = load_settings(
            root_path.as_deref(),
            None::<(SettingsSource, serde_json::Value)>,
            self.home_dir.as_deref(),
            |var| std::env::var(var).ok(),
        );
        self.notifier().log_settings_events(&outcome.events).await;
        let (raw, settings) = if let Some(settings) = outcome.settings {
            (
                outcome
                    .raw_settings
                    .unwrap_or_else(|| crate::config::RawWorkspaceSettings::from(&settings)),
                settings,
            )
        } else {
            let raw = crate::config::defaults::default_settings();
            let settings = WorkspaceSettings::try_from_settings(
                &raw,
                self.home_dir.as_deref(),
                crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
            )
            .unwrap_or_default();
            (raw, settings)
        };
        self.apply_initial_settings(raw, settings).await;
    }
}
