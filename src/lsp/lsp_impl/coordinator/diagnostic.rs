use crate::config::settings::ResolvedAggregationConfig;
use crate::document::DocumentStore;
use crate::language::InjectionResolver;
use crate::language::LanguageCoordinator;
use crate::lsp::aggregation::server::{expand_priorities, truncate_entries};
use crate::lsp::bridge::{BridgeCoordinator, ResolvedServerConfig};
use crate::lsp::debounced_diagnostics::DebouncedDiagnosticsManager;
use crate::lsp::lsp_impl::bridge_context::resolve_aggregation_config_from_settings;
use crate::lsp::lsp_impl::bridge_context::{DocumentRequestContext, HostRequestContext};
use url::Url;

use crate::lsp::lsp_impl::Kakehashi;
use crate::lsp::lsp_impl::text_document::publish_diagnostic::{
    DiagnosticSnapshot, DiagnosticSnapshotLineage, PullLayerOutcome, collect_push_diagnostics,
};
use crate::lsp::settings_manager::SettingsManager;
use crate::lsp::synthetic_diagnostics::SyntheticDiagnosticsManager;

/// Whether the proactive pull's **config-level** server selection for a method
/// is non-empty — the visible walk resolved exactly as dispatch does (expand
/// `priorities`, then truncate by `maxFanOut`). The walk already drops
/// unconfigured names and empty `Rest` groups, so a non-empty walk is exactly a
/// non-empty selection; checking `is_empty()` avoids `select_servers`' config
/// map + `ResolvedServerConfig` clones. An empty selection (`priorities = []`,
/// `maxFanOut = 0`, or names only unconfigured servers) means the pull would fan
/// out to nobody, so the caller skips the context entirely.
///
/// This is a *selection* check, not a contribution prediction: a non-empty
/// selection still builds a context and runs the pull even if every selected
/// server is capability-gated at runtime (`send_diagnostic_request` /
/// `send_host_raw_request` return empty when a server lacks
/// `textDocument/diagnostic`), leaving a present-but-**empty** `PullLayer`. That
/// is harmless for the pull/push dedup (#425): a capability-gated server is, by
/// definition, *push*-driven, so `filter_pull_driven_push_slots` never
/// suppresses its spontaneous push against that empty blob. The skip here only
/// stops a *config*-empty selection from minting an empty `PullLayer` that would
/// falsely suppress a genuinely pull-driven server's push.
fn dispatches_to_any_server(
    priorities: &[String],
    configs: &[ResolvedServerConfig],
    max_fan_out: Option<usize>,
) -> bool {
    !truncate_entries(expand_priorities(priorities, configs), max_fan_out).is_empty()
}

#[derive(Clone)]
pub(crate) struct DiagnosticSnapshotPreparer {
    language: std::sync::Arc<LanguageCoordinator>,
    documents: std::sync::Arc<DocumentStore>,
    bridge: std::sync::Arc<BridgeCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    cache: std::sync::Arc<crate::lsp::cache::CacheCoordinator>,
}

impl DiagnosticSnapshotPreparer {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            language: std::sync::Arc::clone(&server.language),
            documents: std::sync::Arc::clone(&server.documents),
            bridge: std::sync::Arc::clone(&server.bridge),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            cache: std::sync::Arc::clone(&server.cache),
        }
    }
}

pub(crate) struct DiagnosticScheduler {
    documents: std::sync::Arc<DocumentStore>,
    bridge: std::sync::Arc<BridgeCoordinator>,
    settings_manager: std::sync::Arc<SettingsManager>,
    debounced_diagnostics: std::sync::Arc<DebouncedDiagnosticsManager>,
    synthetic_diagnostics: std::sync::Arc<SyntheticDiagnosticsManager>,
    snapshot_preparer: DiagnosticSnapshotPreparer,
    /// The single proactive publisher: the host-event pull below feeds its
    /// result into the cache and republishes (push-propagation-diagnostic-forwarding),
    /// rather than calling `client.publish_diagnostics` directly, so it can never
    /// clobber a sibling region's push.
    publisher: std::sync::Arc<super::DiagnosticPublisher>,
}

impl DiagnosticScheduler {
    pub(crate) fn new(server: &Kakehashi) -> Self {
        Self {
            documents: std::sync::Arc::clone(&server.documents),
            bridge: std::sync::Arc::clone(&server.bridge),
            settings_manager: std::sync::Arc::clone(&server.settings_manager),
            debounced_diagnostics: std::sync::Arc::clone(&server.debounced_diagnostics),
            synthetic_diagnostics: std::sync::Arc::clone(&server.synthetic_diagnostics),
            snapshot_preparer: DiagnosticSnapshotPreparer::new(server),
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

        // Apply the current `diagnostics_debounce_ms` setting (#533). The snapshot
        // is a cheap arc-swap clone, so reading it per schedule keeps the debounce
        // in sync with config reloads without a dedicated apply hook.
        self.debounced_diagnostics.set_debounce_millis(
            self.settings_manager
                .load_settings()
                .diagnostics_debounce_ms,
        );

        self.debounced_diagnostics.schedule(
            uri,
            snapshot_data,
            self.bridge.pool_arc(),
            std::sync::Arc::clone(&self.bridge),
            std::sync::Arc::clone(&self.synthetic_diagnostics),
            std::sync::Arc::clone(&self.publisher),
            std::sync::Arc::clone(&self.documents),
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
    /// contribute — skip), `Some(snapshot)` with no pull contributors (the
    /// collection returns `Clear`, evicting any stale `PullLayer`; a
    /// configured-but-pull-gated host still lands here so its re-sync runs), and
    /// `Some(snapshot)` with virt regions and/or a pullable host context.
    pub(crate) fn prepare_diagnostic_snapshot(&self, uri: &Url) -> Option<DiagnosticSnapshot> {
        self.snapshot_preparer.prepare_diagnostic_snapshot(uri)
    }
}

impl DiagnosticSnapshotPreparer {
    pub(crate) fn prepare_diagnostic_snapshot(&self, uri: &Url) -> Option<DiagnosticSnapshot> {
        let (snapshot, language_name, content_version) = {
            let doc = self.documents.get(uri)?;
            let snapshot = doc.snapshot()?;
            let language_name = self.language.detect_language(
                uri.path(),
                snapshot.text(),
                None,
                doc.language_id(),
            )?;
            let content_version = doc.content_version();
            (snapshot, language_name, content_version)
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
                        // Prefer the populate pass's regions riding the current
                        // parse snapshot (never discover twice, ADR §3); fall
                        // back to the inline resolution when absent/stale.
                        let all_regions = match self
                            .documents
                            .current_resolved_regions(uri, self.cache.semantic_token_generation())
                        {
                            Some(regions) => regions,
                            None => std::sync::Arc::new(InjectionResolver::resolve_all(
                                &self.language,
                                self.bridge.node_tracker(),
                                uri,
                                snapshot.tree(),
                                snapshot.text(),
                                injection_query.as_ref(),
                                snapshot.incarnation(),
                            )),
                        };

                        let mut contexts = Vec::new();
                        // Configs + aggregation are keyed by injection language, so
                        // resolve them once per distinct language (a document can
                        // hold many regions of one language). `None` caches a skipped
                        // language: no configured server, OR the pull would dispatch
                        // to none. The key is cloned only on the resolving miss, not
                        // on every region.
                        type ResolvedLang =
                            Option<(Vec<ResolvedServerConfig>, ResolvedAggregationConfig)>;
                        let mut resolved_by_lang: std::collections::HashMap<String, ResolvedLang> =
                            std::collections::HashMap::new();
                        for resolved in all_regions.iter() {
                            // `get` on the common (cache-hit) path is a single lookup;
                            // only the resolving miss touches the map again, cloning
                            // the language key just for that insert.
                            let entry = match resolved_by_lang.get(&resolved.injection_language) {
                                Some(entry) => entry,
                                None => {
                                    let lang = &resolved.injection_language;
                                    let computed: ResolvedLang = {
                                        let configs = self.bridge.get_all_configs_for_language(
                                            &settings,
                                            &language_name,
                                            lang,
                                        );
                                        if configs.is_empty() {
                                            None
                                        } else {
                                            let agg = resolve_aggregation_config_from_settings(
                                                &settings,
                                                &language_name,
                                                lang,
                                                "textDocument/publishDiagnostics",
                                            );
                                            // Only keep a language the pull will actually
                                            // dispatch (#425): drop it when `pullFallback =
                                            // false` OR its effective server selection is
                                            // empty (`priorities = []`, `maxFanOut = 0`, or
                                            // names only unconfigured servers). This keeps
                                            // the invariant "PullLayer present ⟺ a pull
                                            // dispatched to ≥1 server", so an absent/Clear
                                            // pull layer never falsely suppresses a
                                            // server's spontaneous push. The push path is
                                            // untouched — only kakehashi's pulling stops.
                                            if !agg.pull_fallback
                                                || !dispatches_to_any_server(
                                                    &agg.priorities,
                                                    &configs,
                                                    agg.max_fan_out,
                                                )
                                            {
                                                None
                                            } else {
                                                Some((configs, agg))
                                            }
                                        }
                                    };
                                    resolved_by_lang
                                        .entry(resolved.injection_language.clone())
                                        .or_insert(computed)
                                }
                            };
                            let Some((configs, agg)) = entry else {
                                continue;
                            };

                            contexts.push(DocumentRequestContext {
                                uri: uri.clone(),
                                resolved: resolved.clone(),
                                configs: configs.clone(),
                                upstream_request_id: None,
                                priorities: agg.priorities.clone(),
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
        // The host context is built whenever the layer is **configured** (in the
        // priorities AND `_self` opted in with servers), *independent of*
        // `pullFallback`: it carries the text the #431 debounced re-sync pushes
        // to push-only `_self` servers (the host analogue of the virtual-doc
        // didChange forward), which must keep flowing even when the pull is off.
        // `host_pull_enabled` separately gates the Path A pull — false when
        // `pullFallback = false` or the host's effective selection is empty —
        // mirroring the per-region gate above. Building the context (rather than
        // `None`-ing it) also keeps the snapshot non-`None` so an all-gated host's
        // stale `PullLayer` is cleared on this event.
        let host_lang_settings = if layer_cfg.allows(crate::config::settings::LayerSource::Host) {
            settings
                .resolve_host_language_settings(&language_name)
                .filter(|lang_settings| lang_settings.is_host_bridging_enabled())
        } else {
            None
        };
        let mut host_pull_enabled = false;
        let host = host_lang_settings.and_then(|lang_settings| {
            let configs = self
                .bridge
                .get_host_configs_for_language(&settings, &language_name);
            if configs.is_empty() {
                return None;
            }
            let agg = lang_settings.resolve_host_aggregation("textDocument/publishDiagnostics");
            host_pull_enabled = agg.pull_fallback
                && dispatches_to_any_server(&agg.priorities, &configs, agg.max_fan_out);
            Some(HostRequestContext {
                uri: uri.clone(),
                language_id: language_name.clone(),
                text: snapshot.text_arc(),
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
        // `PullLayer` is cleared on this event and its re-sync still runs.
        let virt_contexts = match (virt_contexts, host.is_some()) {
            (None, false) => return None,
            (virt, _) => virt.unwrap_or_default(),
        };

        Some(DiagnosticSnapshot {
            lineage: DiagnosticSnapshotLineage {
                incarnation: snapshot.incarnation(),
                content_version,
            },
            virt_contexts,
            host_pull_enabled,
            host,
            layer_cfg,
        })
    }
}

/// Logging target for synthetic push diagnostics.
const LOG_TARGET: &str = "kakehashi::synthetic_diag";
