//! Upstream `workspace/didChangeWorkspaceFolders` lifecycle handling.

use tower_lsp_server::ls_types::DidChangeWorkspaceFoldersParams;

use crate::config::WorkspaceSettings;
use crate::lsp::{SettingsSource, load_settings};

use super::super::{Kakehashi, lock_settings_reload};

impl Kakehashi {
    pub(crate) async fn did_change_workspace_folders_impl(
        &self,
        params: DidChangeWorkspaceFoldersParams,
    ) {
        let added = params.event.added;
        let removed = params.event.removed;
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
            return;
        }

        // Reading the settings in effect, merging the reloaded root onto them,
        // and publishing the result share the one reload transaction
        // `workspace/didChangeConfiguration` uses. Without it a configuration
        // push racing this notification can publish a merge derived from the
        // snapshot the other has already replaced.
        let reload = lock_settings_reload().await;

        let root_path = self
            .bridge
            .pool()
            .workspace_folders()
            .and_then(|folders| folders.first().cloned())
            .and_then(|folder| super::super::uri_to_url(&folder.uri).ok())
            .and_then(|url| url.to_file_path().ok());
        self.settings_manager.set_root_path(root_path);

        // An explicit `--config-file` replaces the whole config-file stack, so
        // the project layer at the workspace root is never consulted and no
        // file layer can change when the root does. Those files are also read
        // exactly once by contract, which leaves this reload nothing to read
        // and only initialize's settings events to re-emit. Refreshing the root
        // — which still anchors relative paths — and stopping there is the whole
        // of it; #746 covers reloading the project layer for the sessions that
        // do have one.
        if crate::config::expand::config_file_override().is_some() {
            self.request_folder_change_repull();
            return;
        }

        let root_path = self.settings_manager.root_path().as_ref().clone();
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
        WorkspaceFoldersChangeEvent,
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
}
