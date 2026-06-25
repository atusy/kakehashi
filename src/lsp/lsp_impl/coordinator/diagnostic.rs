use crate::document::DocumentStore;
use crate::language::InjectionResolver;
use crate::language::LanguageCoordinator;
use crate::lsp::bridge::BridgeCoordinator;
use crate::lsp::debounced_diagnostics::DebouncedDiagnosticsManager;
use crate::lsp::lsp_impl::bridge_context::resolve_aggregation_config_from_settings;
use crate::lsp::lsp_impl::bridge_context::{DocumentRequestContext, HostRequestContext};
use url::Url;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::lsp_impl::text_document::publish_diagnostic::{
    DiagnosticSnapshot, PullLayerOutcome, collect_push_diagnostics,
};
use crate::lsp::settings_manager::SettingsManager;
use crate::lsp::synthetic_diagnostics::SyntheticDiagnosticsManager;

pub(crate) struct DiagnosticScheduler {
    language: std::sync::Arc<LanguageCoordinator>,
    documents: std::sync::Arc<DocumentStore>,
    bridge: std::sync::Arc<BridgeCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    debounced_diagnostics: std::sync::Arc<DebouncedDiagnosticsManager>,
    synthetic_diagnostics: std::sync::Arc<SyntheticDiagnosticsManager>,
    /// The single proactive publisher: the host-event pull below feeds its
    /// result into the cache and republishes (push-propagation-diagnostic-forwarding),
    /// rather than calling `client.publish_diagnostics` directly, so it can never
    /// clobber a sibling region's push.
    publisher: std::sync::Arc<super::DiagnosticPublisher>,
}

impl DiagnosticScheduler {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            language: std::sync::Arc::clone(&server.language),
            documents: std::sync::Arc::clone(&server.documents),
            bridge: std::sync::Arc::clone(&server.bridge),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            debounced_diagnostics: std::sync::Arc::clone(&server.debounced_diagnostics),
            synthetic_diagnostics: std::sync::Arc::clone(&server.synthetic_diagnostics),
            publisher: std::sync::Arc::new(super::DiagnosticPublisher::new(server)),
        }
    }

    /// Schedule a debounced diagnostic for a document (pull-first-diagnostic-forwarding Phase 3).
    ///
    /// A later change cancels and replaces the pending timer. The snapshot is
    /// captured now, at schedule time, so diagnostics stay consistent with the
    /// document state that triggered the change.
    pub(crate) fn schedule_debounced_diagnostic(&self, uri: Url) {
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);

        self.debounced_diagnostics.schedule(
            uri,
            snapshot_data,
            self.bridge.pool_arc(),
            std::sync::Arc::clone(&self.bridge),
            std::sync::Arc::clone(&self.synthetic_diagnostics),
            std::sync::Arc::clone(&self.publisher),
        );
    }

    /// Spawn a background task to collect and publish diagnostics.
    ///
    /// pull-first-diagnostic-forwarding Phase 2: synthetic push on didSave/didOpen. The task registers
    /// with `SyntheticDiagnosticsManager` (superseding any previous task), then
    /// fans out to downstream servers and publishes via
    /// `textDocument/publishDiagnostics`.
    pub(crate) fn spawn_synthetic_diagnostic_task(&self, uri: Url) {
        let snapshot_data = self.prepare_diagnostic_snapshot(&uri);
        let bridge_pool = self.bridge.pool_arc();
        let publisher = std::sync::Arc::clone(&self.publisher);
        let uri_clone = uri.clone();

        let task = tokio::spawn(async move {
            let diagnostics =
                collect_push_diagnostics(snapshot_data, &bridge_pool, &uri_clone, LOG_TARGET).await;

            // Feed the host-event pull outcome into the cache and republish the
            // merged set (push-propagation-diagnostic-forwarding) instead of
            // publishing directly — push slots for the same host survive.
            match diagnostics {
                PullLayerOutcome::Skip => {}
                PullLayerOutcome::Clear => publisher.clear_pull_layer(&uri_clone).await,
                PullLayerOutcome::Publish(diagnostics) => {
                    log::debug!(
                        target: LOG_TARGET,
                        "Collected {} pull-layer diagnostics for {}",
                        diagnostics.len(),
                        uri_clone
                    );
                    publisher.publish_pull_layer(&uri_clone, diagnostics).await;
                }
            }
        });

        self.synthetic_diagnostics
            .register_task(uri, task.abort_handle());
    }

    /// Prepare the per-layer diagnostic snapshot for a background task
    /// (cross-layer-aggregation).
    ///
    /// Extracts all data synchronously before spawning to avoid lifetime issues
    /// with `self` references in async tasks. Return states: `None` (document
    /// missing, no snapshot, no language, or nothing that could ever
    /// contribute — skip publishing), `Some(snapshot)` with no contributors
    /// (caller publishes empty to clear), and `Some(snapshot)` with
    /// virt regions and/or a host context ready for requests.
    pub(crate) fn prepare_diagnostic_snapshot(&self, uri: &Url) -> Option<DiagnosticSnapshot> {
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

        // Cross-layer gating, keyed by the same method name as the
        // aggregation configs below. A layer gated off still yields a
        // snapshot (with that layer absent) so that publishing an empty list
        // clears anything a previously-enabled configuration left behind.
        let settings = self.settings_manager.load_settings();
        let layer_cfg = crate::lsp::lsp_impl::bridge_context::resolve_layer_config_from_settings(
            &settings,
            &language_name,
            "textDocument/publishDiagnostics",
        );

        // Virt layer: `None` = the document can never have virt diagnostics
        // (no injection query), distinct from `Some(vec![])` = gated off or
        // currently no regions (publish-empty-to-clear).
        let virt_contexts: Option<Vec<DocumentRequestContext>> =
            if !layer_cfg.allows(crate::config::settings::LayerSource::Virt) {
                log::debug!(
                    target: LOG_TARGET,
                    "virt layer disabled for {} via layers.aggregation priorities",
                    language_name
                );
                Some(Vec::new())
            } else {
                self.language
                    .injection_query(&language_name)
                    .map(|injection_query| {
                        let all_regions = InjectionResolver::resolve_all(
                            &self.language,
                            self.bridge.node_tracker(),
                            uri,
                            snapshot.tree(),
                            snapshot.text(),
                            injection_query.as_ref(),
                        );

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

                            // `pullFallback = false` drops this region's
                            // pull-driven servers from the proactive path: the
                            // host-event pull does not fan out to them (Path A,
                            // #425). Their spontaneous pushes are still cached and
                            // published; only kakehashi's pulling stops. An empty
                            // contexts list still publishes-empty to clear any
                            // stale pull contribution.
                            if !agg.pull_fallback {
                                continue;
                            }

                            contexts.push(DocumentRequestContext {
                                uri: uri.clone(),
                                resolved,
                                configs,
                                upstream_request_id: None,
                                priorities: agg.priorities,
                                strategy: agg.strategy,
                                max_fan_out: agg.max_fan_out,
                                client_progress_token: None,
                            });
                        }
                        contexts
                    })
            };

        // Host layer (host-document-bridge): participates when listed in the
        // layer priorities AND opted in via bridge._self.enabled with a
        // host-capable server. No upstream request id — push tasks are not
        // tied to a client request.
        //
        // `host_bridging_configured` is that participation *independent of*
        // `pullFallback`; `host` is the pull context, which `pullFallback` can
        // gate to `None` while the layer stays configured. The split matters so
        // an all-gated host still yields a snapshot whose Clear outcome evicts a
        // stale `PullLayer` (#425) rather than skipping and leaving it to linger.
        let host_lang_settings = if layer_cfg.allows(crate::config::settings::LayerSource::Host) {
            settings
                .resolve_host_language_settings(&language_name)
                .filter(|lang_settings| lang_settings.is_host_bridging_enabled())
        } else {
            None
        };
        let mut host_bridging_configured = false;
        let host = host_lang_settings.and_then(|lang_settings| {
            let configs = self
                .bridge
                .get_host_configs_for_language(&settings, &language_name);
            if configs.is_empty() {
                return None;
            }
            host_bridging_configured = true;
            let agg = lang_settings.resolve_host_aggregation("textDocument/publishDiagnostics");
            // `pullFallback = false` skips the host-layer pull (Path A, #425); a
            // push-driven `_self` server still publishes via its cached pushes,
            // and a pull-only `_self` server simply stops being pulled on host
            // events. The layer stays configured (above), so the snapshot is
            // still produced and its Clear outcome evicts any stale pull blob.
            if !agg.pull_fallback {
                return None;
            }
            Some(HostRequestContext {
                uri: uri.clone(),
                language_id: language_name.clone(),
                text: std::sync::Arc::from(snapshot.text()),
                configs,
                priorities: agg.priorities,
                strategy: agg.strategy,
                max_fan_out: agg.max_fan_out,
                upstream_request_id: None,
            })
        });

        // A document that can never contribute (no injection query AND no
        // configured host layer) keeps the old skip-publishing behavior. A
        // configured-but-pull-gated host still produces a snapshot so its stale
        // `PullLayer` is cleared on this event.
        let virt_contexts = match (virt_contexts, host.is_some() || host_bridging_configured) {
            (None, false) => return None,
            (virt, _) => virt.unwrap_or_default(),
        };

        Some(DiagnosticSnapshot {
            virt_contexts,
            host,
            layer_cfg,
        })
    }
}

/// Logging target for synthetic push diagnostics.
const LOG_TARGET: &str = "kakehashi::synthetic_diag";
