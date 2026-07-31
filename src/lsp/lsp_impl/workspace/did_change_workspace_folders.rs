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

        // The change moved the project every client-fallback downstream
        // analyses, so what they report can differ while no document — and no
        // document version — did. A pull-namespace editor has no event of its
        // own for that, and the lineage the call above just dropped means an
        // answer crossing the change resolves to an empty layer rather than the
        // baseline. Both are repaired by the same nudge.
        //
        // FORCED because the coverage gate suppresses an unforced refresh when
        // nothing is dirty by version — the exact shape of a change that moves
        // the project rather than the documents. Requested here rather than
        // after the reload because the `--config-file` branch returns before
        // it, never reaching a later call site. Capability-gated and
        // single-flighted inside; `forced` bypasses neither.
        //
        // The request is fire-and-forget, so the editor's re-pull can land
        // while the reload below still holds the pre-reload settings. That is
        // benign — the root path is read only where settings are loaded, never
        // at spawn — and a reload that does change something recycles through
        // `propagate_settings` and nudges again on its own.
        super::super::coordinator::DiagnosticPublisher::new(self)
            .request_pull_diagnostic_refresh(true);

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
    }
}
