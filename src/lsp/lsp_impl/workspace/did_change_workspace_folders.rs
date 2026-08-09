//! Upstream `workspace/didChangeWorkspaceFolders` lifecycle handling.

use tower_lsp_server::ls_types::DidChangeWorkspaceFoldersParams;

use crate::config::WorkspaceSettings;
use crate::lsp::{SettingsSource, load_settings};

use super::super::{Kakehashi, lifecycle::config_root_after_folder_change, lock_settings_reload};

impl Kakehashi {
    pub(crate) async fn did_change_workspace_folders_impl(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        let added = params.event.added;
        let removed = params.event.removed;

        // Reading the settings in effect, merging the reloaded root onto them,
        // and publishing the result share the one reload transaction
        // `workspace/didChangeConfiguration` uses. Without it a configuration
        // push racing this notification can publish a merge derived from the
        // snapshot the other has already replaced.
        //
        // The pool's folder set moves inside that transaction too: it is what
        // the configuration root is derived from, so committing it first would
        // let a push anchor against the old root after the workspace has
        // already moved. The lock order is reload-then-connections on every
        // path that takes both, since the settings reload also reaches the
        // bridge. Taken even for an event that may turn out to name no
        // folder: the pool's own emptiness check below returns before it
        // ever reaches `connections`, so nothing is held across that check.
        let reload = lock_settings_reload().await;

        // The pool owns the definition of "this event changed something" and
        // reports it, so this reload cannot drift from that definition by
        // duplicating its own emptiness check. An event that named no folder
        // moved no project: re-deriving the settings root from it would drop
        // the project config layer for a session whose folder list is empty,
        // and reparsing every open document plus a semantic-tokens refresh is
        // a high price for a notification that said nothing.
        if !self
            .bridge
            .pool()
            .apply_workspace_folder_change(added, &removed)
            .await
        {
            drop(reload);
            return;
        }

        // An emptied folder list does not leave the session rootless when the
        // client named another root: the rungs below `workspaceFolders` answer,
        // as they did at initialize. A client that named none gets no project
        // layer rather than the launch directory.
        let first_folder = self
            .bridge
            .pool()
            .workspace_folders()
            .and_then(|folders| folders.first().cloned());
        let root_path = config_root_after_folder_change(
            first_folder.as_ref().map(|folder| &folder.uri),
            self.settings_manager.folderless_root_path(),
        );

        // An explicit `--config-file` replaces the whole config-file stack, so
        // the project layer at the workspace root is never consulted and no
        // file layer can change when the root does. Those files are also read
        // exactly once by contract, which leaves this reload nothing to read
        // and only initialize's settings events to re-emit. Refreshing the root
        // — which still anchors relative paths — and stopping there is the whole
        // of it.
        if crate::config::expand::config_file_override().is_some() {
            self.settings_manager.set_root_path(root_path);
            return;
        }

        // The root stays local until the settings derived from it are the ones
        // in effect. Publishing it earlier would leave a rejected reload with
        // the new root over the old snapshot, so the next pushed layer would
        // anchor to a workspace the settings in effect know nothing about.
        let client_override = self.client_settings_override.load_full();
        let outcome = load_settings(
            root_path.as_deref(),
            client_override.as_deref().and_then(|settings| {
                serde_json::to_value(settings)
                    .ok()
                    .map(|value| (SettingsSource::InitializationOptions, value))
            }),
            self.home_dir.as_deref(),
            |var| std::env::var(var).ok(),
            // The branch above returned for every session that has one.
            None,
        );
        self.notifier().log_settings_events(&outcome.events).await;
        let raw = outcome
            .raw_settings
            .unwrap_or_else(crate::config::defaults::default_settings);
        match WorkspaceSettings::try_from_settings(
            &raw,
            self.home_dir.as_deref(),
            crate::config::expand::with_kakehashi_defaults(|var| std::env::var(var).ok()),
        ) {
            Ok(settings) => {
                let warnings = Self::misconfigured_settings_warnings(&settings);
                self.settings_manager.set_root_path(root_path);
                self.apply_raw_settings_locked(&reload, raw, settings).await;
                drop(reload);
                self.warn_on_misconfigured_settings(&warnings).await;
            }
            Err(error) => {
                drop(reload);
                self.notifier()
                    .log_warning(format!(
                        "Workspace root changed, but reloaded settings were invalid: {error}"
                    ))
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::time::Duration;
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{WorkspaceFolder, WorkspaceFoldersChangeEvent};

    /// An event naming neither an addition nor a removal describes no change
    /// of project: `apply_workspace_folder_change` already reports as much
    /// (see `pool.rs`), and this handler owns the expensive half of reacting
    /// to a folder change — the settings reload, which invalidates every open
    /// document's parse tree and pushes a workspace-wide
    /// `semanticTokens/refresh`. Paying that cost for a notification that
    /// changed nothing is wasteful.
    ///
    /// Asserted via the settings snapshot's own identity rather than a new
    /// counter: `apply_raw_settings_locked` always publishes a fresh `Arc`
    /// through `SettingsManager::apply_settings_with_raw`, regardless of
    /// whether the content actually differs (see `apply_shared_settings_locked`
    /// in `lsp_impl.rs`), so an unmoved pointer is proof the whole reload
    /// transaction — `load_settings`, `WorkspaceSettings::try_from_settings`,
    /// `apply_raw_settings_locked` — never ran.
    ///
    /// Bounded so a regression fails on the assertion rather than hanging: an
    /// unskipped reload reaches client notifications this harness's unused
    /// `_socket` never drains.
    #[tokio::test]
    async fn an_empty_folder_event_does_not_reload_settings() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let before = server.settings_manager.load_settings_pair();

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: Vec::new(),
                    removed: Vec::new(),
                },
            }),
        )
        .await
        .expect("an event that names no folder must return without a reload");

        let after = server.settings_manager.load_settings_pair();
        assert!(
            Arc::ptr_eq(&before, &after),
            "an event that names no folder must not run the settings-reload \
             transaction at all"
        );
    }

    /// Regression guard for the branch above: a real folder change must still
    /// reach the reload. `WorkspaceSettings::try_from_settings` succeeds for
    /// the default settings this session starts with, so the `Ok` arm runs
    /// `apply_raw_settings_locked`, which republishes a fresh snapshot even
    /// though its *content* is unchanged from the default.
    #[tokio::test]
    async fn a_non_empty_folder_event_still_reloads_settings() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let before = server.settings_manager.load_settings_pair();

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![WorkspaceFolder {
                        uri: "file:///added".parse().unwrap(),
                        name: "added".to_string(),
                    }],
                    removed: Vec::new(),
                },
            }),
        )
        .await
        .expect("a real folder change must not hang the reload");

        let after = server.settings_manager.load_settings_pair();
        assert!(
            !Arc::ptr_eq(&before, &after),
            "a real folder change must still run the settings-reload transaction"
        );
    }
}
