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

pub(crate) struct DiagnosticScheduler {
    client: Client,
    language: std::sync::Arc<LanguageCoordinator>,
    documents: std::sync::Arc<DocumentStore>,
    bridge: std::sync::Arc<BridgeCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    debounced_diagnostics: std::sync::Arc<DebouncedDiagnosticsManager>,
    synthetic_diagnostics: std::sync::Arc<SyntheticDiagnosticsManager>,
}

impl DiagnosticScheduler {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            client: server.client.clone(),
            language: std::sync::Arc::clone(&server.language),
            documents: std::sync::Arc::clone(&server.documents),
            bridge: std::sync::Arc::clone(&server.bridge),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            debounced_diagnostics: std::sync::Arc::clone(&server.debounced_diagnostics),
            synthetic_diagnostics: std::sync::Arc::clone(&server.synthetic_diagnostics),
        }
    }

    /// Schedule a debounced diagnostic for a document (pull-first-diagnostic-forwarding Phase 3).
    ///
    /// A later change cancels and replaces the pending timer. The snapshot is
    /// captured now, at schedule time, so diagnostics stay consistent with the
    /// document state that triggered the change.
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
    /// pull-first-diagnostic-forwarding Phase 2: synthetic push on didSave/didOpen. The task registers
    /// with `SyntheticDiagnosticsManager` (superseding any previous task), then
    /// fans out to downstream servers and publishes via
    /// `textDocument/publishDiagnostics`.
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
    /// Extracts all data synchronously before spawning to avoid lifetime issues
    /// with `self` references in async tasks. The three return states are
    /// distinct: `None` (document missing, no snapshot, or no
    /// language/injection config), `Some(Vec::new())` (no injection regions —
    /// caller should clear diagnostics), and `Some(vec![...])` (regions ready
    /// for requests).
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

        // Layer gate (cross-layer-aggregation), keyed by the same method name
        // as the aggregation config below. Returning an empty snapshot (not
        // None) publishes an empty diagnostics list, clearing anything a
        // previously-enabled configuration left in the editor.
        let settings = self.settings_manager.load_settings();
        if !crate::lsp::lsp_impl::bridge_context::resolve_layer_config_from_settings(
            &settings,
            &language_name,
            "textDocument/publishDiagnostics",
        )
        .allows(crate::config::settings::LayerSource::Virt)
        {
            log::debug!(
                target: LOG_TARGET,
                "virt layer disabled for {} via layers.order",
                language_name
            );
            return Some(Vec::new());
        }

        let injection_query = self.language.injection_query(&language_name)?;

        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.node_tracker(),
            uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Some(Vec::new());
        }

        let mut contexts = Vec::new();
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
