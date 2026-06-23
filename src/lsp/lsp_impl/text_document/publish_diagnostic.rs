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
    /// Host-layer context (host-document-bridge); `None` unless the host
    /// layer is in `layers.aggregation` priorities AND `bridge._self` is
    /// opted in with a capable server.
    pub(crate) host: Option<HostRequestContext>,
    /// Cross-layer combine config for `textDocument/publishDiagnostics`.
    pub(crate) layer_cfg: ResolvedLayerConfig,
}

impl DiagnosticSnapshot {
    /// Whether any layer can contribute diagnostics.
    pub(crate) fn has_contributors(&self) -> bool {
        !self.virt_contexts.is_empty() || self.host.is_some()
    }
}

/// Collect diagnostics from every participating layer using priority-aware
/// aggregation (cross-layer-aggregation).
///
/// Shared logic for both immediate (didSave/didOpen) and debounced (didChange)
/// push diagnostics. Returns `None` if there's no snapshot data, or
/// `Some(diagnostics)` (possibly empty to clear previous diagnostics).
pub(crate) async fn collect_push_diagnostics(
    snapshot_data: Option<DiagnosticSnapshot>,
    pool: &Arc<LanguageServerPool>,
    uri: &Url,
    log_target: &'static str,
) -> Option<Vec<tower_lsp_server::ls_types::Diagnostic>> {
    let snapshot = snapshot_data?;

    if !snapshot.has_contributors() {
        log::debug!(
            target: log_target,
            "No diagnostic contributors (regions or host servers) for {}",
            uri
        );
        // Return empty to signal caller should clear diagnostics
        return Some(Vec::new());
    }

    // Destructure so each future owns exactly the field it needs (the
    // async blocks would otherwise rely on disjoint-field captures of
    // `snapshot`, which compiles but reads ambiguously).
    let DiagnosticSnapshot {
        virt_contexts,
        host,
        layer_cfg,
    } = snapshot;

    let virt_fut = async {
        let mut join_set = JoinSet::new();
        for region_ctx in virt_contexts {
            let pool = Arc::clone(pool);
            join_set.spawn(async move { collect_region_diagnostics(&region_ctx, pool).await });
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
            Some(ctx) => collect_host_diagnostics(ctx, Arc::clone(pool)).await,
            None => Vec::new(),
        }
    };

    let (virt_items, host_items) = tokio::join!(virt_fut, host_fut);

    Some(combine_layer_diagnostics(
        &layer_cfg, virt_items, host_items,
    ))
}
