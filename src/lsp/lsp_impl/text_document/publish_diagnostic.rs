//! Shared fan-out/aggregation for the host-event diagnostic pull
//! (push-propagation-diagnostic-forwarding): on `didOpen`/`didSave`/`didChange`,
//! `DiagnosticScheduler` pulls every layer's diagnostics from a prepared snapshot
//! and the result is fed into the cache as the `PullLayer` blob, then republished.
//! `DiagnosticScheduler` handles superseding (via `SyntheticDiagnosticsManager` /
//! `DebouncedDiagnosticsManager`); this module just collects the per-layer
//! diagnostics.

use std::sync::Arc;

use tokio::task::JoinSet;
use url::Url;

use crate::config::settings::ResolvedLayerConfig;
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::{DocumentRequestContext, HostRequestContext};

use super::diagnostic::{
    collect_host_diagnostics, collect_region_diagnostics, combine_layer_diagnostics,
};

/// Everything a push-diagnostics task needs, captured at schedule time
/// (cross-layer-aggregation): the virt layer's per-region contexts, the host
/// layer's context when host bridging participates, and the resolved
/// cross-layer config that combines them.
pub(crate) struct DiagnosticSnapshot {
    /// Per-region virt contexts; empty when the virt layer is gated off or
    /// the document has no bridgeable injection regions.
    pub(crate) virt_contexts: Vec<DocumentRequestContext>,
    /// Host-layer context (host-document-bridge); `None` unless the host layer
    /// is in `layers.aggregation` priorities AND `bridge._self` is opted in with
    /// a configured server. Present even when the host **pull** is gated off
    /// (see `host_pull_enabled`) because it also carries the text the #431
    /// debounced re-sync pushes to push-only `_self` servers.
    pub(crate) host: Option<HostRequestContext>,
    /// Whether the host context should be **pulled** on this event (Path A).
    /// `false` when host `pullFallback = false` or the host's effective server
    /// selection is empty; the context still drives the re-sync. Always `false`
    /// when `host` is `None`.
    pub(crate) host_pull_enabled: bool,
    /// Cross-layer combine config for `textDocument/publishDiagnostics`.
    pub(crate) layer_cfg: ResolvedLayerConfig,
}

impl DiagnosticSnapshot {
    /// Whether any layer can contribute to the **pull** this event — the
    /// Publish-vs-Clear decision for the `PullLayer`. The host counts only when
    /// it will actually be pulled (`host_pull_enabled`); a configured-but-gated
    /// host context is for the re-sync, not the pull.
    pub(crate) fn has_contributors(&self) -> bool {
        !self.virt_contexts.is_empty() || (self.host.is_some() && self.host_pull_enabled)
    }
}

/// What the host-event pull collection wants done to the host's `PullLayer`
/// slot (push-propagation-diagnostic-forwarding). The three states are
/// distinct: `Skip` ≠ `Clear` (do nothing vs evict).
pub(crate) enum PullLayerOutcome {
    /// No snapshot (document gone / can never contribute): do nothing, leave the
    /// cache untouched.
    Skip,
    /// Contributors exist but none can pull this event (every layer is
    /// `pullFallback`-gated, or there are genuinely none): **evict** the host's
    /// `PullLayer` so a stale pull blob does not linger AND an absent (not
    /// merely empty) `PullLayer` lets a pull-driven server's spontaneous push
    /// publish — the `pullFallback = false` guarantee (#425). Distinct from a
    /// pull that ran and returned clean (that keeps an empty `PullLayer` present
    /// so the clean result still suppresses a pull-driven server's stale push).
    Clear,
    /// A pull ran; publish its (possibly empty) result as the `PullLayer` blob.
    Publish(Vec<tower_lsp_server::ls_types::Diagnostic>),
}

/// Collect diagnostics from every participating layer using priority-aware
/// aggregation (cross-layer-aggregation).
///
/// Shared logic for both immediate (didSave/didOpen) and debounced (didChange)
/// push diagnostics. Returns a [`PullLayerOutcome`]: `Skip` when there is no
/// snapshot, `Clear` when nothing can pull this event (evict a stale pull blob),
/// or `Publish` with the pull's combined result.
pub(crate) async fn collect_push_diagnostics(
    snapshot_data: Option<DiagnosticSnapshot>,
    pool: &Arc<LanguageServerPool>,
    uri: &Url,
    log_target: &'static str,
) -> PullLayerOutcome {
    let Some(snapshot) = snapshot_data else {
        return PullLayerOutcome::Skip;
    };

    if !snapshot.has_contributors() {
        log::debug!(
            target: log_target,
            "No pull contributors for {} (no pullable regions, and any host layer is \
             pullFallback-gated or empty-selection); clearing pull layer",
            uri
        );
        // Evict (not publish-empty): an absent PullLayer both clears any stale
        // pull result and avoids falsely triggering the pull/push double-count
        // filter against a pull-driven server's spontaneous push (#425).
        return PullLayerOutcome::Clear;
    }

    // Destructure so each future owns exactly the field it needs (the
    // async blocks would otherwise rely on disjoint-field captures of
    // `snapshot`, which compiles but reads ambiguously).
    let DiagnosticSnapshot {
        mut virt_contexts,
        host,
        host_pull_enabled,
        layer_cfg,
    } = snapshot;

    // Drop servers already known (a live, `Ready` connection) NOT to answer pull
    // diagnostics before spawning their per-region tasks
    // (capability-prefilter-fanout). The proactive push path
    // (didOpen/didSave/debounced didChange) fans out exactly like the editor's
    // pull, so a push-only server (e.g. basedpyright, warmed by eager-open)
    // would otherwise be re-dispatched once per region on every change. The
    // per-region `send_diagnostic_request` already returns an empty layer for
    // such a server (its capability gate), so with the default (untruncated)
    // `maxFanOut` pre-dropping only removes the wasted task — the collected set
    // is unchanged. Under a configured `maxFanOut` this shares the same
    // priority/truncation aggregation as the pull path, so dropping an incapable
    // high-priority server can promote the next capable one into a kept slot —
    // an answer where the region previously collected empty (the improvement
    // noted in the PR description), never a regression.
    if !virt_contexts.is_empty() {
        let candidates: std::collections::HashSet<&str> = virt_contexts
            .iter()
            .flat_map(|ctx| ctx.configs.iter().map(|c| c.server_name.as_str()))
            .collect();
        let incapable = pool
            .servers_known_incapable(&candidates, "textDocument/diagnostic")
            .await;
        if !incapable.is_empty() {
            for ctx in &mut virt_contexts {
                ctx.configs.retain(|c| !incapable.contains(&c.server_name));
            }
            virt_contexts.retain(|ctx| !ctx.configs.is_empty());
        }
    }

    let virt_fut = async {
        let mut join_set = JoinSet::new();
        for region_ctx in virt_contexts {
            let pool = Arc::clone(pool);
            // Push diagnostics run in LSP mode only — failures are log-only
            // (the editor re-pulls), so no request-error sink is threaded.
            join_set
                .spawn(async move { collect_region_diagnostics(&region_ctx, pool, &None).await });
        }

        let mut all_diagnostics = Vec::new();
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(diags) => all_diagnostics.extend(diags),
                Err(join_err) => {
                    log::warn!(
                        target: log_target,
                        "Diagnostic region task panicked: {}",
                        join_err
                    );
                }
            }
        }
        all_diagnostics
    };

    let host_fut = async {
        match &host {
            // Pull only when enabled; a configured-but-gated host context exists
            // for the re-sync (above), not the pull. Push diagnostics are
            // LSP-mode-only, so failures are log-only (`&None` error sink).
            Some(ctx) if host_pull_enabled => {
                collect_host_diagnostics(ctx, Arc::clone(pool), &None).await
            }
            _ => Vec::new(),
        }
    };

    let (virt_items, host_items) = tokio::join!(virt_fut, host_fut);

    PullLayerOutcome::Publish(combine_layer_diagnostics(
        &layer_cfg, virt_items, host_items,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::{AggregationStrategy, BridgeServerConfig};
    use crate::language::injection::{CacheableInjectionRegion, ResolvedInjection};
    use crate::lsp::bridge::ConnectionState;
    use crate::lsp::bridge::ResolvedServerConfig;
    use crate::lsp::bridge::test_helpers::create_handle_with_state;

    fn virt_ctx_for_server(server: &str) -> DocumentRequestContext {
        DocumentRequestContext {
            uri: Url::parse("file:///t.md").unwrap(),
            resolved: ResolvedInjection {
                region: CacheableInjectionRegion {
                    language: "python".to_string(),
                    byte_range: 0..0,
                    line_range: 0..0,
                    start_column: 0,
                    region_id: "r0".to_string(),
                    content_hash: 0,
                },
                raw_injection_language: "python".to_string(),
                injection_language: "python".to_string(),
                virtual_content: String::new(),
                line_column_offsets: vec![],
            },
            configs: vec![ResolvedServerConfig {
                server_name: server.to_string(),
                config: Arc::new(BridgeServerConfig {
                    cmd: vec![server.to_string()],
                    languages: vec![],
                    initialization_options: None,
                    workspace_markers: None,
                    on_type_formatting_triggers: None,
                    prefer_shared_instance: None,
                    enabled: None,
                    settings: None,
                }),
            }],
            upstream_request_id: None,
            priorities: vec![],
            strategy: AggregationStrategy::Preferred,
            max_fan_out: None,
            client_progress_token: None,
        }
    }

    /// An all-incapable virt snapshot must still publish an (empty) pull layer,
    /// NOT `Clear`: `has_contributors()` runs BEFORE the capability prefilter, so
    /// dropping every server leaves the same empty publish the per-region
    /// capability gate would have produced (capability-prefilter-fanout). Guards
    /// the ordering against a refactor that filters ahead of that check (which
    /// would turn a self-healing empty publish into a stale-clearing eviction).
    #[tokio::test]
    async fn all_incapable_virt_snapshot_publishes_empty_not_clear() {
        let pool = Arc::new(LanguageServerPool::new());
        // A `Ready` connection for server "test" with no advertised capabilities
        // → known-incapable of `textDocument/diagnostic`, so the prefilter drops
        // it and the region's config set empties.
        let handle = create_handle_with_state(ConnectionState::Ready).await;
        pool.insert_connection(handle).await;

        let snapshot = DiagnosticSnapshot {
            virt_contexts: vec![virt_ctx_for_server("test")],
            host: None,
            host_pull_enabled: false,
            layer_cfg: ResolvedLayerConfig::with_defaults("textDocument/publishDiagnostics"),
        };

        let uri = Url::parse("file:///t.md").unwrap();
        let outcome = collect_push_diagnostics(Some(snapshot), &pool, &uri, "test").await;

        match outcome {
            PullLayerOutcome::Publish(diags) => {
                assert!(diags.is_empty(), "expected an empty publish");
            }
            PullLayerOutcome::Clear => {
                panic!("all-incapable virt snapshot must Publish(empty), not Clear")
            }
            PullLayerOutcome::Skip => panic!("snapshot was Some, must not Skip"),
        }
    }
}
