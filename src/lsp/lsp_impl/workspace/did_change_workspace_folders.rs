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

    use serial_test::serial;
    use std::sync::Arc;
    use std::time::Duration;
    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{WorkspaceFolder, WorkspaceFoldersChangeEvent};

    /// Restores `XDG_CONFIG_HOME` when dropped, even if the test body panics —
    /// matching the convention in `src/config/user.rs` / `src/lsp/settings.rs`.
    /// Callers must carry `#[serial(xdg_env)]`, since the variable is
    /// process-wide.
    struct XdgConfigHomeGuard(Option<std::ffi::OsString>);

    impl XdgConfigHomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let original = std::env::var_os("XDG_CONFIG_HOME");
            // SAFETY: #[serial(xdg_env)] prevents concurrent modification of
            // XDG_CONFIG_HOME.
            unsafe { std::env::set_var("XDG_CONFIG_HOME", path) };
            Self(original)
        }
    }

    impl Drop for XdgConfigHomeGuard {
        fn drop(&mut self) {
            // SAFETY: #[serial(xdg_env)] prevents concurrent modification of
            // XDG_CONFIG_HOME.
            unsafe {
                match self.0.take() {
                    Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                    None => std::env::remove_var("XDG_CONFIG_HOME"),
                }
            }
        }
    }

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
    /// `XDG_CONFIG_HOME` is isolated even though the *current* early return
    /// never reaches `load_settings`: this test exists to catch a regression
    /// that removes that early return, and a false pass is exactly what a
    /// developer machine's real `~/.config/kakehashi/kakehashi.toml` could
    /// produce in that case — e.g. an invalid real config makes
    /// `WorkspaceSettings::try_from_settings` fail before publishing a new
    /// snapshot, leaving `Arc::ptr_eq` true for the wrong reason and hiding
    /// the very regression this test is meant to catch.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn an_empty_folder_event_does_not_reload_settings() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());

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
    ///
    /// `XDG_CONFIG_HOME` is pointed at an empty scratch directory for the
    /// duration of the call. Unlike the sibling test above, this path DOES
    /// reach `load_settings`, which reads `$XDG_CONFIG_HOME` (falling back to
    /// `~/.config`) for a real user config file — left unisolated, a
    /// developer machine's own `~/.config/kakehashi/kakehashi.toml` loads
    /// instead of empty defaults, and its real search paths / language
    /// configuration can turn this call into actual disk work whose outcome
    /// (and duration, past the 5s bound below) depends on whoever's machine
    /// runs it, rather than on the code under test.
    ///
    /// The added folder is a real scratch directory rather than a bare
    /// `file:///added`, for the same reason: `load_settings` also reads
    /// `<root>/kakehashi.toml` from the folder it derives as the project
    /// root, and a literal `/added` risks resolving to a real top-level
    /// directory (and a real config file inside it) on some host.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_non_empty_folder_event_still_reloads_settings() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let workspace_dir = tempfile::tempdir().expect("failed to create scratch workspace dir");
        // `Url::from_directory_path` percent-encodes reserved characters and
        // handles platform path quirks (e.g. Windows drive letters) that a
        // bare `format!("file://{}", ...)` would mangle if the scratch path
        // ever contained one — `$TMPDIR` is not guaranteed plain-ASCII.
        let folder_uri = url::Url::from_directory_path(workspace_dir.path())
            .expect("scratch workspace dir must convert to a file:// URL");

        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        let before = server.settings_manager.load_settings_pair();

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![WorkspaceFolder {
                        uri: folder_uri.as_str().parse().unwrap(),
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
