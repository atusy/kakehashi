//! Upstream `workspace/didChangeWorkspaceFolders` lifecycle handling.

use std::path::PathBuf;

use tower_lsp_server::ls_types::{DidChangeWorkspaceFoldersParams, WorkspaceFolder};

use super::super::{Kakehashi, uri_to_url};
use crate::lsp::settings::SettingsLoadOutcome;
use crate::lsp::{SettingsSource, load_settings};

fn primary_root_path(
    folders: Option<&[WorkspaceFolder]>,
    fallback: Option<PathBuf>,
) -> Option<PathBuf> {
    folders
        .and_then(|folders| folders.first())
        .and_then(|folder| uri_to_url(&folder.uri).ok())
        .and_then(|url| url.to_file_path().ok())
        .or(fallback)
}

fn load_primary_root_settings(
    root_path: Option<&std::path::Path>,
    session_settings: Option<crate::config::RawWorkspaceSettings>,
    home: Option<&str>,
) -> SettingsLoadOutcome {
    let session_override = session_settings
        .and_then(|settings| serde_json::to_value(settings).ok())
        .map(|settings| (SettingsSource::SessionOverrides, settings));
    load_settings(root_path, session_override, home, |var| {
        std::env::var(var).ok()
    })
}

impl Kakehashi {
    pub(crate) async fn did_change_workspace_folders_impl(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        let folders = self
            .bridge
            .pool()
            .apply_workspace_folder_change(params.event.added, &params.event.removed);
        let root_path = primary_root_path(
            folders.as_deref(),
            self.settings_manager.fallback_root_path(),
        );
        if self.settings_manager.root_path().as_ref() == &root_path {
            return;
        }

        let outcome = load_primary_root_settings(
            root_path.as_deref(),
            self.settings_manager.load_session_settings(),
            self.home_dir.as_deref(),
        );
        self.notifier().log_settings_events(&outcome.events).await;

        let (Some(raw_settings), Some(settings)) = (outcome.raw_settings, outcome.settings) else {
            self.notifier()
                .log_warning(
                    "Workspace root changed, but its configuration could not be applied; \
                     retaining the previous root and settings",
                )
                .await;
            return;
        };
        self.apply_raw_settings_at_root(root_path, raw_settings, settings)
            .await;
        self.notifier()
            .log_info("Primary workspace root configuration updated")
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RawWorkspaceSettings, merge_workspace_settings};
    use std::str::FromStr;
    use tower_lsp_server::ls_types::Uri;

    fn folder(uri: &str, name: &str) -> WorkspaceFolder {
        WorkspaceFolder {
            uri: Uri::from_str(uri).expect("valid URI"),
            name: name.to_string(),
        }
    }

    #[test]
    fn primary_root_uses_first_folder_in_client_order() {
        let folders = [
            folder("file:///first", "first"),
            folder("file:///second", "second"),
        ];

        assert_eq!(
            primary_root_path(Some(&folders), Some(PathBuf::from("/fallback"))),
            Some(PathBuf::from("/first"))
        );
    }

    #[test]
    fn primary_root_falls_back_when_last_folder_is_removed() {
        assert_eq!(
            primary_root_path(Some(&[]), Some(PathBuf::from("/fallback"))),
            Some(PathBuf::from("/fallback"))
        );
    }

    #[test]
    fn root_reload_preserves_initialization_and_runtime_layers() {
        let root = tempfile::tempdir().expect("temporary root");
        std::fs::write(
            root.path().join("kakehashi.toml"),
            "diagnosticsDebounceMs = 321\n",
        )
        .expect("project config");
        let initialization = RawWorkspaceSettings {
            auto_install: Some(false),
            ..Default::default()
        };
        let runtime = RawWorkspaceSettings {
            search_paths: Some(vec!["/runtime/parsers".to_string()]),
            ..Default::default()
        };
        let session = merge_workspace_settings(Some(initialization), Some(runtime));

        let outcome = load_primary_root_settings(Some(root.path()), session, None);
        let raw = outcome.raw_settings.expect("settings load");

        assert_eq!(raw.diagnostics_debounce_ms, Some(321));
        assert_eq!(raw.auto_install, Some(false));
        assert_eq!(raw.search_paths, Some(vec!["/runtime/parsers".to_string()]));
    }
}
