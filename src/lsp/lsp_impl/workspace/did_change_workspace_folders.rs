//! Upstream `workspace/didChangeWorkspaceFolders` lifecycle handling.

use tower_lsp_server::ls_types::DidChangeWorkspaceFoldersParams;

use crate::error::LockResultExt;

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

        // The change above recycles client-fallback connections, which is
        // exactly where a `forceStart` warm-up lives — it has no document, so
        // no marker root, so no other key. Re-assert it here rather than
        // relying on the settings application below: two paths return before
        // reaching it, and a warm-up that nothing re-acquires (a policy server
        // with `languages = []` has no document to trigger one) would
        // otherwise stay dead for the session. Idempotent per key, so the
        // later application re-asserting it costs nothing.
        self.bridge
            .force_start_servers(&self.settings_manager.load_settings());

        // An emptied folder list does not leave the session rootless when the
        // client named another root: the rungs below `workspaceFolders` answer,
        // as they did at initialize. A client that named none gets no project
        // layer rather than the launch directory.
        let first_folder = self
            .bridge
            .pool()
            .workspace_folders()
            .and_then(|folders| folders.first().cloned());
        let (root_path, root_scope) = config_root_after_folder_change(
            first_folder.as_ref().map(|folder| &folder.uri),
            (
                self.settings_manager.folderless_root_path(),
                self.settings_manager.folderless_root_scope(),
            ),
        );

        // The root stays local until the settings derived from it are the ones
        // in effect. Publishing it earlier would leave a rejected reload with
        // the new root over the old snapshot, so the next pushed layer would
        // anchor to a workspace the settings in effect know nothing about.
        let client_layers = self
            .client_layers
            .read()
            .recover_poison("client_layers replay")
            .to_fold_order();
        match self
            .recompose_settings(root_path.as_deref(), client_layers)
            .await
        {
            Ok(super::recompose::Recomposed {
                raw,
                settings,
                base,
            }) => {
                // The files were read for the new root: the prefix a later
                // client-layer rebuild resumes from is theirs now.
                *self
                    .settings_base
                    .write()
                    .recover_poison("settings_base root change") = base;
                let warnings = Self::misconfigured_settings_warnings(&settings);
                let root_changed = *self.settings_manager.root_path() != root_path
                    || self.settings_manager.root_scope() != root_scope;
                self.settings_manager.set_root(root_path, root_scope);
                self.apply_raw_settings_locked(&reload, raw, settings).await;
                drop(reload);
                self.warn_on_misconfigured_settings(&warnings).await;
                // The client's configuration was asked for the old root's
                // scope, and does not describe the new one. Asked only once the
                // new root is in effect, so the answer names and anchors to it;
                // a rejected reload keeps the old root, where an answer would
                // anchor to a workspace the editor has left. Until it arrives,
                // the old answer stays in effect: withdrawing it here would run
                // this reload without the client's configuration, respawning
                // every bridge server configured only through the editor.
                // Awaited like the pull a no-payload `didChangeConfiguration`
                // triggers, under the same timeout and single-flight.
                if root_changed {
                    self.pull_client_configuration().await;
                }
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

    fn folder(dir: &std::path::Path, name: &str) -> WorkspaceFolder {
        let uri = url::Url::from_directory_path(dir)
            .expect("scratch workspace dir must convert to a file:// URL");
        WorkspaceFolder {
            uri: uri.as_str().parse().unwrap(),
            name: name.to_string(),
        }
    }

    /// Drive `initialize` through the real service — a client may only be
    /// sent requests once it is initialized — with one workspace folder and a
    /// client that can answer `workspace/configuration`. Every server→client
    /// request is answered — a configuration pull with `[answer]` — and the
    /// `workspace/configuration` params are recorded.
    async fn initialized_pull_capable_server(
        first: &std::path::Path,
        answer: serde_json::Value,
    ) -> (
        LspService<Kakehashi>,
        Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        initialized_server_answering(serde_json::json!([folder(first, "first")]), vec![answer])
            .await
    }

    /// [`initialized_pull_capable_server`], with the `workspaceFolders` sent
    /// at `initialize` spelled out and one answer per pull, in order — the
    /// last one repeating once they run out.
    async fn initialized_server_answering(
        workspace_folders: serde_json::Value,
        answers: Vec<serde_json::Value>,
    ) -> (
        LspService<Kakehashi>,
        Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        initialized_server_holding_answers(workspace_folders, answers, None).await
    }

    /// [`initialized_server_answering`], but the first pull is answered only
    /// once `release_first` is notified — so a test can move the session
    /// while that answer is in flight.
    async fn initialized_server_holding_answers(
        workspace_folders: serde_json::Value,
        answers: Vec<serde_json::Value>,
        release_first: Option<Arc<tokio::sync::Notify>>,
    ) -> (
        LspService<Kakehashi>,
        Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    ) {
        use futures::{SinkExt, StreamExt};
        use tower::{Service, ServiceExt};
        use tower_lsp_server::jsonrpc::{Request, Response};

        let (mut service, socket) = LspService::new(Kakehashi::new);
        let pulls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&pulls);
        let (mut requests, mut responses) = socket.split();
        tokio::spawn(async move {
            while let Some(request) = requests.next().await {
                let Some(id) = request.id().cloned() else {
                    continue;
                };
                let result = if request.method() == "workspace/configuration" {
                    let count = {
                        let mut recorded = recorded.lock().unwrap();
                        recorded.push(request.params().cloned().unwrap_or_default());
                        recorded.len()
                    };
                    if count == 1
                        && let Some(release) = release_first.as_ref()
                    {
                        release.notified().await;
                    }
                    let answer = answers
                        .get(count - 1)
                        .or(answers.last())
                        .cloned()
                        .unwrap_or_default();
                    serde_json::json!([answer])
                } else {
                    serde_json::Value::Null
                };
                if responses.send(Response::from_ok(id, result)).await.is_err() {
                    return;
                }
            }
        });

        // After the responder is running: `initialize` logs to the client,
        // and nothing would drain those messages otherwise.
        let initialize = Request::build("initialize")
            .params(serde_json::json!({
                "capabilities": { "workspace": {
                    "configuration": true,
                    "workspaceFolders": true,
                } },
                "workspaceFolders": workspace_folders,
            }))
            .id(1)
            .finish();
        service
            .ready()
            .await
            .unwrap()
            .call(initialize)
            .await
            .expect("initialize must succeed");
        (service, pulls)
    }

    /// A pull-capable client is asked again once the selected configuration
    /// root moves: the settings in effect were read for the old workspace.
    ///
    /// The answer is a real layer rather than `null`, so it is applied — which
    /// takes the reload lock the folder change held, and anchors its relative
    /// path to the root now in effect.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_root_change_pulls_the_client_configuration_again() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create first workspace dir");
        let second = tempfile::tempdir().expect("failed to create second workspace dir");

        let (service, pulls) = initialized_pull_capable_server(
            first.path(),
            serde_json::json!({ "searchPaths": ["./pulled"] }),
        )
        .await;
        let server = service.inner();

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![folder(second.path(), "second")],
                    removed: vec![folder(first.path(), "first")],
                },
            }),
        )
        .await
        .expect("a root change must not hang");

        assert_eq!(
            pulls.lock().unwrap().len(),
            1,
            "a root change must ask the client for its configuration again"
        );
        let pulled = second.path().join("pulled");
        assert!(
            server
                .settings_manager
                .load_settings()
                .search_paths
                .iter()
                .any(|path| std::path::Path::new(path) == pulled),
            "the answer must be applied, anchored to the new root"
        );
    }

    /// Adding a folder behind the first one leaves the selected root where it
    /// was, so the configuration already read still describes it.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_folder_change_that_keeps_the_root_does_not_pull() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create first workspace dir");
        let second = tempfile::tempdir().expect("failed to create second workspace dir");

        let (service, pulls) =
            initialized_pull_capable_server(first.path(), serde_json::Value::Null).await;
        let server = service.inner();

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![folder(second.path(), "second")],
                    removed: Vec::new(),
                },
            }),
        )
        .await
        .expect("a folder change must not hang");

        assert!(
            pulls.lock().unwrap().is_empty(),
            "a folder change that keeps the selected root must not pull"
        );
    }

    /// Trigger a pull the way a pull-model editor does: a
    /// `didChangeConfiguration` carrying no payload.
    async fn pull_now(server: &Kakehashi) {
        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_configuration_impl(
                tower_lsp_server::ls_types::DidChangeConfigurationParams {
                    settings: serde_json::Value::Null,
                },
            ),
        )
        .await
        .expect("a pull must not hang");
    }

    fn scope_of(pull: &serde_json::Value) -> Option<&str> {
        pull["items"][0]["scopeUri"].as_str()
    }

    /// The pull names the selected configuration root as its scope, so the
    /// client answers for the workspace the settings are resolved against —
    /// and a root change asks for the new root's scope.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn the_pull_is_scoped_to_the_selected_root() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create first workspace dir");
        let second = tempfile::tempdir().expect("failed to create second workspace dir");

        let (service, pulls) =
            initialized_pull_capable_server(first.path(), serde_json::Value::Null).await;
        let server = service.inner();

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_configuration_impl(
                tower_lsp_server::ls_types::DidChangeConfigurationParams {
                    settings: serde_json::Value::Null,
                },
            ),
        )
        .await
        .expect("a pull must not hang");
        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![folder(second.path(), "second")],
                    removed: vec![folder(first.path(), "first")],
                },
            }),
        )
        .await
        .expect("a root change must not hang");

        let pulls = pulls.lock().unwrap();
        let scopes = pulls.iter().map(scope_of).collect::<Vec<_>>();
        assert_eq!(
            scopes,
            vec![
                Some(folder(first.path(), "first").uri.as_str()),
                Some(folder(second.path(), "second").uri.as_str()),
            ],
            "each pull must name the root selected when it was asked"
        );
    }

    /// A root kakehashi fell back to on its own — the launch directory, for a
    /// client that named no workspace — is not the client's to scope by, so
    /// the pull asks for the client's global configuration.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_session_without_a_client_root_pulls_unscoped() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());

        let (service, pulls) =
            initialized_server_answering(serde_json::Value::Null, vec![serde_json::Value::Null])
                .await;
        let server = service.inner();
        assert!(
            server.settings_manager.root_path().is_some(),
            "precondition: the launch directory stands in as the root"
        );

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_configuration_impl(
                tower_lsp_server::ls_types::DidChangeConfigurationParams {
                    settings: serde_json::Value::Null,
                },
            ),
        )
        .await
        .expect("a pull must not hang");

        let pulls = pulls.lock().unwrap();
        assert_eq!(pulls.len(), 1);
        assert_eq!(scope_of(&pulls[0]), None);
    }

    fn language_server(name: &str) -> serde_json::Value {
        serde_json::json!({ "languageServers": { name: { "cmd": [name], "languages": ["zz"] } } })
    }

    fn has_language_server(server: &Kakehashi, name: &str) -> bool {
        server
            .settings_manager
            .load_settings()
            .language_servers
            .contains_key(name)
    }

    /// A pull answer is the client's whole configuration for the scope, not a
    /// delta: a newer answer takes the older one's place rather than
    /// accumulating over it, so a server the client stopped configuring goes.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_newer_pull_answer_replaces_the_previous_one() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create workspace dir");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(first.path(), "first")]),
            vec![language_server("old-server"), language_server("new-server")],
        )
        .await;
        let server = service.inner();

        pull_now(server).await;
        assert!(has_language_server(server, "old-server"), "precondition");
        pull_now(server).await;

        assert!(has_language_server(server, "new-server"));
        assert!(
            !has_language_server(server, "old-server"),
            "the older answer must not survive beneath the newer one"
        );
    }

    /// An answer that holds nothing for kakehashi — only keys the editor keeps
    /// in the same section — says the client configures nothing here now, so
    /// it withdraws what the previous answer configured.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn an_answer_with_nothing_for_kakehashi_withdraws_the_previous_one() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create workspace dir");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(first.path(), "first")]),
            vec![
                language_server("old-server"),
                serde_json::json!({ "trace": { "server": "off" } }),
            ],
        )
        .await;
        let server = service.inner();

        pull_now(server).await;
        assert!(has_language_server(server, "old-server"), "precondition");
        pull_now(server).await;

        assert!(!has_language_server(server, "old-server"));
    }

    /// `null` is the client saying it cannot answer, not that it configures
    /// nothing: the previous answer stays in effect.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_null_answer_keeps_the_previous_one() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create workspace dir");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(first.path(), "first")]),
            vec![language_server("old-server"), serde_json::Value::Null],
        )
        .await;
        let server = service.inner();

        pull_now(server).await;
        pull_now(server).await;

        assert!(has_language_server(server, "old-server"));
    }

    /// The newest answer is the newest statement of the client's
    /// configuration, so it lands above pushes that arrived before it rather
    /// than back where the previous answer sat.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_pull_answer_lands_above_older_pushes() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create workspace dir");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(first.path(), "first")]),
            vec![
                serde_json::json!({ "searchPaths": ["/pulled-first"] }),
                serde_json::json!({ "searchPaths": ["/pulled-second"] }),
            ],
        )
        .await;
        let server = service.inner();

        pull_now(server).await;
        server
            .did_change_configuration_impl(
                tower_lsp_server::ls_types::DidChangeConfigurationParams {
                    settings: serde_json::json!({ "kakehashi": { "searchPaths": ["/pushed"] } }),
                },
            )
            .await;
        pull_now(server).await;

        assert_eq!(
            server.settings_manager.load_settings().search_paths,
            vec!["/pulled-second".to_string()]
        );
    }

    /// An answer asked for one root and arriving after the session moved to
    /// another describes a workspace no longer selected: it is dropped, and
    /// the root change's own pull asks for the new scope instead.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn an_answer_for_a_root_the_session_left_is_discarded() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create first workspace dir");
        let second = tempfile::tempdir().expect("failed to create second workspace dir");
        let release_first = Arc::new(tokio::sync::Notify::new());

        let (service, pulls) = initialized_server_holding_answers(
            serde_json::json!([folder(first.path(), "first")]),
            vec![
                serde_json::json!({ "searchPaths": ["./stale"] }),
                serde_json::Value::Null,
            ],
            Some(Arc::clone(&release_first)),
        )
        .await;
        let server = service.inner();

        let move_root_while_answering = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while pulls.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the first pull must be asked");
            server
                .did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![folder(second.path(), "second")],
                        removed: vec![folder(first.path(), "first")],
                    },
                })
                .await;
            release_first.notify_one();
        };
        tokio::join!(pull_now(server), move_root_while_answering);

        let scopes = pulls
            .lock()
            .unwrap()
            .iter()
            .map(|pull| scope_of(pull).map(str::to_owned))
            .collect::<Vec<_>>();
        assert_eq!(
            scopes.last().cloned().flatten().as_deref(),
            Some(folder(second.path(), "second").uri.as_str()),
            "the root change must still ask for the new scope"
        );
        assert!(
            !server
                .settings_manager
                .load_settings()
                .search_paths
                .iter()
                .any(|path| path.ends_with("stale")),
            "an answer for the root the session left must not be applied"
        );
    }

    /// A session that started on the launch directory and then gains a
    /// folder at that same path has moved from a root kakehashi chose to one
    /// the client named: the path is unchanged, but the scope a pull names is
    /// not, so the client is asked again — for that folder.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn gaining_a_folder_at_the_launch_directory_pulls_for_it() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());

        let (service, pulls) =
            initialized_server_answering(serde_json::Value::Null, vec![serde_json::Value::Null])
                .await;
        let server = service.inner();
        let launch_directory = server
            .settings_manager
            .root_path()
            .as_ref()
            .clone()
            .expect("precondition: the launch directory stands in as the root");

        tokio::time::timeout(
            Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: vec![folder(&launch_directory, "launch")],
                    removed: Vec::new(),
                },
            }),
        )
        .await
        .expect("a folder change must not hang");

        let pulls = pulls.lock().unwrap();
        assert_eq!(
            pulls.iter().map(scope_of).collect::<Vec<_>>(),
            vec![Some(folder(&launch_directory, "launch").uri.as_str())],
            "the client-named root must be asked for, though its path is unchanged"
        );
    }

    /// A pull rebuilds the settings to take the previous answer out, but the
    /// configuration files were read when the root was selected and are not
    /// read again for it: a project file saved half-edited in between must not
    /// silently drop out of effect because the editor's settings changed.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn a_pull_does_not_reread_the_configuration_files() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let workspace = tempfile::tempdir().expect("failed to create workspace dir");
        let project_file = workspace.path().join("kakehashi.toml");
        std::fs::write(&project_file, "searchPaths = [\"/from-project\"]\n")
            .expect("failed to write the project config");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(workspace.path(), "workspace")]),
            vec![language_server("old-server"), language_server("new-server")],
        )
        .await;
        let server = service.inner();
        let from_project = |server: &Kakehashi| {
            server
                .settings_manager
                .load_settings()
                .search_paths
                .iter()
                .any(|path| path == "/from-project")
        };

        pull_now(server).await;
        assert!(
            from_project(server),
            "precondition: the project file applies"
        );

        std::fs::write(&project_file, "searchPaths = [\n").expect("failed to break the config");
        pull_now(server).await;

        assert!(
            has_language_server(server, "new-server"),
            "the answer applies"
        );
        assert!(
            from_project(server),
            "the project file read at the root change must stay in effect"
        );
    }

    /// An answer holding only keys the editor keeps in the same section, with
    /// no earlier answer to withdraw, changes nothing — so nothing is rebuilt
    /// or republished, however the emptiness is spelled.
    #[rstest::rstest]
    #[case::empty_section(serde_json::json!({}))]
    #[case::editor_keys_only(serde_json::json!({ "trace": { "server": "off" } }))]
    #[tokio::test]
    #[serial(xdg_env)]
    async fn an_answer_with_nothing_to_withdraw_changes_nothing(#[case] answer: serde_json::Value) {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create workspace dir");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(first.path(), "first")]),
            vec![answer],
        )
        .await;
        let server = service.inner();
        let before = server.settings_manager.load_settings_pair();

        pull_now(server).await;

        assert!(
            Arc::ptr_eq(&before, &server.settings_manager.load_settings_pair()),
            "an answer that changes no layer must not republish the settings"
        );
    }

    /// The discard compares scopes as well as paths: an unscoped answer asked
    /// on the launch directory, arriving after a folder at that same path was
    /// added, was read for the client's global settings rather than for that
    /// folder — the path alone cannot tell the two apart.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn an_answer_for_a_scope_the_session_left_is_discarded() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let release_first = Arc::new(tokio::sync::Notify::new());

        let (service, pulls) = initialized_server_holding_answers(
            serde_json::Value::Null,
            vec![
                serde_json::json!({ "searchPaths": ["/stale"] }),
                serde_json::Value::Null,
            ],
            Some(Arc::clone(&release_first)),
        )
        .await;
        let server = service.inner();
        let launch_directory = server
            .settings_manager
            .root_path()
            .as_ref()
            .clone()
            .expect("precondition: the launch directory stands in as the root");

        let add_the_same_path_while_answering = async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while pulls.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the first pull must be asked");
            server
                .did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                    event: WorkspaceFoldersChangeEvent {
                        added: vec![folder(&launch_directory, "launch")],
                        removed: Vec::new(),
                    },
                })
                .await;
            release_first.notify_one();
        };
        tokio::join!(pull_now(server), add_the_same_path_while_answering);

        assert!(
            !server
                .settings_manager
                .load_settings()
                .search_paths
                .iter()
                .any(|path| path == "/stale"),
            "an unscoped answer must not stand in for the folder's"
        );
    }

    /// A pull-model editor asks on every settings change, most of which are
    /// not kakehashi's: an answer identical to the previous one leaves the
    /// settings as they are, rather than republishing them — which would
    /// reparse every open document and refresh semantic tokens for nothing.
    #[tokio::test]
    #[serial(xdg_env)]
    async fn an_unchanged_answer_does_not_republish() {
        let xdg_scratch = tempfile::tempdir().expect("failed to create scratch XDG_CONFIG_HOME");
        let _xdg_guard = XdgConfigHomeGuard::set(xdg_scratch.path());
        let first = tempfile::tempdir().expect("failed to create workspace dir");

        let (service, _pulls) = initialized_server_answering(
            serde_json::json!([folder(first.path(), "first")]),
            vec![language_server("same-server")],
        )
        .await;
        let server = service.inner();

        pull_now(server).await;
        assert!(has_language_server(server, "same-server"), "precondition");
        let before = server.settings_manager.load_settings_pair();
        pull_now(server).await;

        assert!(
            Arc::ptr_eq(&before, &server.settings_manager.load_settings_pair()),
            "an answer that changes nothing must not republish the settings"
        );
    }
}
