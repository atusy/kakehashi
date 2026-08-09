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
        // bridge. Taken even for an event that may turn out to name no folder:
        // the pool's own emptiness check below returns before it ever reaches
        // `connections`, so nothing is held across that check.
        let reload = lock_settings_reload().await;

        // The pool owns the definition of "this event changed something" and
        // reports it, so the reload and the re-pull below cannot drift from the
        // fence's notion of it. An event that named no folder moved no project:
        // re-deriving the settings root from it would drop the project config
        // layer for a session whose folder list is empty, and reparsing every
        // open document plus a semantic-tokens refresh is a high price for a
        // notification that said nothing.
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
        // of it. This branch returns before any reload below, so it is the only
        // chance the pull-namespace nudge gets on this path.
        if crate::config::expand::config_file_override().is_some() {
            self.settings_manager.set_root_path(root_path);
            self.request_folder_change_repull();
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
        // After the reload, not before it. The editor answers a refresh by
        // pulling immediately, and a pull served from the previous project's
        // settings is not merely stale: if the new project drops the server
        // that produced a layer, the reparse that follows yields
        // `PullLayerOutcome::Skip`, which neither clears the layer nor asks for
        // another refresh — so the editor would keep diagnostics from a server
        // the new workspace does not configure. Requested on the error path
        // too: the previous settings remain in effect there, but the folder set
        // and the fence moved regardless, so the editor still owes itself a
        // pull.
        self.request_folder_change_repull();
    }

    /// Ask a pull-namespace editor to re-pull after a workspace-folder change.
    ///
    /// FORCED because the coverage gate suppresses an unforced refresh when
    /// nothing is dirty by version — the exact shape of a change that moves the
    /// project rather than the documents. Capability-gated and single-flighted
    /// inside; `forced` bypasses neither.
    ///
    /// Without it the editor has no event of its own telling it the project
    /// moved, and the lineage the pool just fenced means an answer crossing the
    /// change resolves to an empty layer rather than the baseline. One nudge
    /// repairs both.
    fn request_folder_change_repull(&self) {
        super::super::coordinator::DiagnosticPublisher::new(self)
            .request_pull_diagnostic_refresh(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tower_lsp_server::LspService;
    use tower_lsp_server::ls_types::{
        ClientCapabilities, DiagnosticWorkspaceClientCapabilities, WorkspaceClientCapabilities,
        WorkspaceFolder, WorkspaceFoldersChangeEvent,
    };

    /// A client that advertises `workspace.diagnostics.refreshSupport`, which is
    /// the gate every refresh request passes before it is even counted.
    fn refresh_capable() -> ClientCapabilities {
        ClientCapabilities {
            workspace: Some(WorkspaceClientCapabilities {
                diagnostics: Some(DiagnosticWorkspaceClientCapabilities {
                    refresh_support: Some(true),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn an_empty_folder_event_requests_no_pull_refresh() {
        // Asserted in-process rather than over the wire. tower-lsp runs ingress
        // handlers concurrently, so a request sent after this notification can
        // be handled before it finishes — and `shutdown` in particular cancels
        // the token that suppresses an already-admitted refresh. A wire-level
        // "no refresh arrived" is therefore satisfiable by a regression.
        // `refreshes_requested` is recorded synchronously ahead of every
        // admission gate and cannot be hidden that way.
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();
        server.settings_manager.set_capabilities(refresh_capable());

        // Bounded so a regression fails on the assertion rather than hanging.
        // Without the guard the handler runs on into the settings reload, which
        // parks in this harness on a socket the test never drains — verified by
        // mutating the guard away, where an unbounded await hung instead of
        // reporting the count it had already moved.
        // The result is asserted, not discarded: a handler that stalls BEFORE
        // requesting the refresh would otherwise leave the counter at zero and
        // pass after simply waiting out the bound.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
                event: WorkspaceFoldersChangeEvent {
                    added: Vec::new(),
                    removed: Vec::new(),
                },
            }),
        )
        .await
        .expect("an event that names no folder must return without a reload");

        assert_eq!(
            server.diagnostics.metrics_snapshot().refreshes_requested,
            0,
            "an event that names no folder must not force a workspace-wide re-pull"
        );

        // Proves the zero above is the guard and not a fixture whose capability
        // gate was shut all along — the same request, made directly, counts.
        super::super::super::coordinator::DiagnosticPublisher::new(server)
            .request_pull_diagnostic_refresh(true);
        assert_eq!(
            server.diagnostics.metrics_snapshot().refreshes_requested,
            1,
            "the capability gate must be open for the assertion above to mean anything"
        );
    }

    /// A non-empty event must not touch the pool before it can acquire the
    /// settings-reload lock — reload-then-connections is the order every path
    /// that takes both locks must follow (see the doc comment on
    /// `apply_workspace_folder_change` in `pool.rs`). Racing the handler
    /// against an externally-held `reload` guard is the only way to observe
    /// that ordering rather than just the refresh count: a regression that
    /// moved the lock acquisition to *after* the pool call would leave this
    /// test's earlier assertions unchanged (the pool call and the refresh
    /// still happen eventually) but would let the folder set move while an
    /// unrelated reload is still in flight.
    #[tokio::test]
    async fn a_non_empty_folder_event_waits_for_the_reload_lock_before_touching_the_pool() {
        let (service, _socket) = LspService::new(Kakehashi::new);
        let server = service.inner();

        let folder = WorkspaceFolder {
            uri: "file:///wf-lock-order".parse().unwrap(),
            name: "wf-lock-order".to_string(),
        };

        // Held by this task, not the handler's — a tokio::sync::Mutex blocks a
        // second await regardless of which logical task requests it.
        let outer_reload = lock_settings_reload().await;

        let handler = server.did_change_workspace_folders_impl(DidChangeWorkspaceFoldersParams {
            event: WorkspaceFoldersChangeEvent {
                added: vec![folder],
                removed: Vec::new(),
            },
        });
        tokio::pin!(handler);

        let raced_to_completion = tokio::select! {
            () = &mut handler => true,
            () = tokio::time::sleep(std::time::Duration::from_millis(50)) => false,
        };
        assert!(
            !raced_to_completion,
            "the handler must block on the reload lock rather than run to completion while it is held elsewhere"
        );
        assert!(
            server.bridge.pool().workspace_folders().is_none(),
            "the pool's folder set must not move before the handler can acquire the reload lock"
        );

        drop(outer_reload);

        // Not awaited to completion: past the pool mutation this asserts on,
        // the general reload path writes settings-change notifications to the
        // LSP client socket, which this fixture never drains — the same hang
        // `an_empty_folder_event_requests_no_pull_refresh` avoids by taking
        // the early-return path instead. Polling for the one effect this test
        // cares about still drives `handler` forward without needing the rest
        // of it to resolve.
        let moved = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if server.bridge.pool().workspace_folders().is_some() {
                    return;
                }
                tokio::select! {
                    () = &mut handler => return,
                    () = tokio::time::sleep(std::time::Duration::from_millis(5)) => {}
                }
            }
        })
        .await;
        assert!(
            moved.is_ok(),
            "the pool's folder set must move once the handler could acquire the reload lock"
        );
    }
}
