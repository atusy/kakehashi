use crate::config::settings::AggregationStrategy;
use crate::document::DocumentStore;
use crate::language::InjectionResolver;
use crate::language::LanguageCoordinator;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::debounced_diagnostics::DebouncedDiagnosticsManager;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::lsp_impl::bridge_context::resolve_aggregation_config_from_settings;
use tower_lsp_server::ls_types::Uri;
use url::Url;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::lsp_impl::text_document::publish_diagnostic::collect_push_diagnostics;
use crate::lsp::settings_manager::SettingsManager;
use crate::lsp::synthetic_diagnostics::SyntheticDiagnosticsManager;
use tower_lsp_server::Client;

pub(crate) struct DiagnosticScheduler<'a> {
    client: Client,
    language: &'a std::sync::Arc<LanguageCoordinator>,
    documents: &'a DocumentStore,
    bridge: &'a BridgeCoordinator,
    settings_manager: &'a SettingsManager,
    debounced_diagnostics: &'a DebouncedDiagnosticsManager,
    synthetic_diagnostics: std::sync::Arc<SyntheticDiagnosticsManager>,
}

impl<'a> DiagnosticScheduler<'a> {
    pub(crate) fn new(server: &'a Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: &server.language,
            documents: &server.documents,
            bridge: &server.bridge,
            settings_manager: &server.settings_manager,
            debounced_diagnostics: &server.debounced_diagnostics,
            synthetic_diagnostics: std::sync::Arc::clone(&server.synthetic_diagnostics),
        }
    }

    /// Schedule a debounced diagnostic for a document (ADR-0020 Phase 3).
    ///
    /// This schedules a diagnostic collection to run after a debounce delay.
    /// If another change arrives before the delay expires, the previous timer
    /// is cancelled and a new one is started.
    ///
    /// The diagnostic snapshot is captured immediately (at schedule time) to
    /// ensure consistency with the document state that triggered the change.
    pub(crate) fn schedule_debounced_diagnostic(&self, uri: Url, lsp_uri: Uri) {
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);

        self.debounced_diagnostics.schedule(
            uri,
            lsp_uri,
            self.client.clone(),
            snapshot_data,
            self.bridge.pool_arc(),
            std::sync::Arc::clone(&self.synthetic_diagnostics),
        );
    }

    /// Spawn a background task to collect and publish diagnostics.
    ///
    /// ADR-0020 Phase 2: Synthetic push on didSave/didOpen.
    ///
    /// The task:
    /// 1. Registers itself with `SyntheticDiagnosticsManager` (superseding any previous task)
    /// 2. Collects diagnostics via fan-out to downstream servers
    /// 3. Publishes diagnostics via `textDocument/publishDiagnostics`
    pub(crate) fn spawn_synthetic_diagnostic_task(&self, uri: Url, lsp_uri: Uri) {
        let client = self.client.clone();
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);
        let bridge_pool = self.bridge.pool_arc();
        let uri_clone = uri.clone();

        let task = tokio::spawn(async move {
            let diagnostics =
                collect_push_diagnostics(snapshot_data, &bridge_pool, &uri_clone, LOG_TARGET).await;

            let Some(diagnostics) = diagnostics else {
                return;
            };

            log::debug!(
                target: LOG_TARGET,
                "Collected {} diagnostics for {}",
                diagnostics.len(),
                uri_clone
            );

            client.publish_diagnostics(lsp_uri, diagnostics, None).await;
        });

        self.synthetic_diagnostics
            .register_task(uri, task.abort_handle());
    }

    /// Prepare per-region diagnostic contexts for a background task.
    ///
    /// This extracts all necessary data synchronously before spawning,
    /// avoiding lifetime issues with `self` references in async tasks.
    /// Each region gets its own `DocumentRequestContext` with aggregation
    /// priorities resolved for `"textDocument/publishDiagnostics"`.
    ///
    /// # Returns
    ///
    /// - `None`: Document doesn't exist, has no snapshot, or lacks language/injection configuration
    /// - `Some(Vec::new())`: Document exists but has no injection regions (caller should clear diagnostics)
    /// - `Some(vec![...])`: Injection regions found, ready for diagnostic requests
    ///
    /// Used by both immediate synthetic diagnostics (didSave/didOpen) and
    /// debounced diagnostics (didChange).
    pub(crate) fn prepare_diagnostic_snapshot(
        &self,
        uri: &Url,
    ) -> Option<Vec<DocumentRequestContext>> {
        let (snapshot, language_name) = {
            let doc = self.documents.get(uri)?;
            let snapshot = doc.snapshot()?;
            let language_name = self.language.detect_language(
                uri.path(),
                snapshot.text(),
                None,
                doc.language_id(),
            )?;
            (snapshot, language_name)
        };

        let injection_query = self.language.get_injection_query(&language_name)?;

        let all_regions = InjectionResolver::resolve_all(
            self.language,
            self.bridge.region_id_tracker(),
            uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Some(Vec::new());
        }

        let mut contexts = Vec::new();
        let settings = self.settings_manager.load_settings();
        for resolved in all_regions {
            let configs = self.bridge.get_all_configs_for_language(
                &settings,
                &language_name,
                &resolved.injection_language,
            );
            if configs.is_empty() {
                continue;
            }

            let agg = resolve_aggregation_config_from_settings(
                &settings,
                &language_name,
                &resolved.injection_language,
                "textDocument/publishDiagnostics",
                AggregationStrategy::Concatenated,
            );

            contexts.push(DocumentRequestContext {
                uri: uri.clone(),
                resolved,
                configs,
                upstream_request_id: None,
                priorities: agg.priorities,
                strategy: agg.strategy,
                max_fan_out: agg.max_fan_out,
            });
        }

        Some(contexts)
    }
}

/// Logging target for synthetic push diagnostics.
const LOG_TARGET: &str = "kakehashi::synthetic_diag";
