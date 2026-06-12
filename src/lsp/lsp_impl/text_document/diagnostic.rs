//! `textDocument/diagnostic` (pull): Phase 1 of pull-first-diagnostic-forwarding,
//! with multi-region aggregation via parallel fan-out. Push side lives in
//! `publish_diagnostic.rs`.
//!
//! `tokio::select!` races `CancelForwarder::subscribe()` against result
//! aggregation: a `$/cancelRequest` returns `RequestCancelled`, dropping the
//! `JoinSet` aborts all downstream tasks, and cancels are also forwarded
//! downstream fire-and-forget via middleware.

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
};

use super::super::{Kakehashi, uri_to_url};
use crate::config::settings::{AggregationStrategy, LayerSource, ResolvedLayerConfig};
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, HostFanOutTask, dispatch_concatenated, dispatch_host_concatenated,
    dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::{DocumentRequestContext, HostRequestContext};

// ============================================================================
// Shared diagnostic utilities (used by both pull and push diagnostics)
// ============================================================================

/// Per-request timeout for diagnostic fan-out (pull-first-diagnostic-forwarding).
///
/// Used by both pull diagnostics (textDocument/diagnostic) and
/// synthetic push diagnostics (didSave/didOpen/didChange triggered).
const DIAGNOSTIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Collect diagnostics for a single injection region using strategy-aware dispatch.
///
/// Dispatches to either `dispatch_concatenated` or `dispatch_preferred` based on
/// `ctx.strategy`. Used by push diagnostic collection paths driven by
/// `DiagnosticScheduler` and `execute_debounced_diagnostic` in
/// `debounced_diagnostics.rs`.
///
/// Pull diagnostics (`diagnostic_impl`) use `dispatch_concatenated_diagnostics`
/// and `dispatch_preferred_diagnostics` directly to support per-region
/// strategy selection.
pub(crate) async fn collect_region_diagnostics(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
) -> Vec<Diagnostic> {
    match ctx.strategy {
        AggregationStrategy::Concatenated => dispatch_concatenated_diagnostics(ctx, pool).await,
        AggregationStrategy::Preferred => dispatch_preferred_diagnostics(ctx, pool).await,
    }
}

// ============================================================================
// Pull diagnostics implementation (textDocument/diagnostic)
// ============================================================================

impl Kakehashi {
    pub(crate) async fn diagnostic_impl(
        &self,
        params: DocumentDiagnosticParams,
    ) -> Result<DocumentDiagnosticReportResult> {
        let lsp_uri = params.text_document.uri;

        // Convert ls_types::Uri to url::Url for internal use
        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in diagnostic: {}", lsp_uri.as_str());
            return Ok(empty_diagnostic_report());
        };

        log::trace!("textDocument/diagnostic called for {}", uri);

        // Get document snapshot (minimizes lock duration)
        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("textDocument/diagnostic: No document found for {}", uri);
                return Ok(empty_diagnostic_report());
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!(
                        "textDocument/diagnostic: Document not fully initialized for {}",
                        uri
                    );
                    return Ok(empty_diagnostic_report());
                }
                Some(snapshot) => snapshot,
            },
            // doc automatically dropped here, lock released
        };

        // Get the language for this document
        let Some(language_name) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::diagnostic", "No language detected");
            return Ok(empty_diagnostic_report());
        };

        // Cross-layer gating (cross-layer-aggregation): virt and host
        // participate independently per `layers.aggregation` priorities;
        // host additionally requires the `bridge._self.enabled` opt-in
        // (resolve_host_bridge_context returns None otherwise).
        let layer_cfg = self.resolve_layer_config(&language_name, "textDocument/diagnostic");
        let virt_enabled = layer_cfg.allows(LayerSource::Virt);
        let host_ctx = if layer_cfg.allows(LayerSource::Host) {
            self.resolve_host_bridge_context(&lsp_uri, "textDocument/diagnostic")
        } else {
            None
        };
        if !virt_enabled && host_ctx.is_none() {
            log::debug!(
                target: "kakehashi::diagnostic",
                "no diagnostic layer enabled for {} (layers.aggregation priorities / bridge._self)",
                language_name
            );
            return Ok(empty_diagnostic_report());
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = crate::lsp::current_upstream_id();

        // Subscribe to cancel notifications for this request.
        // The guard ensures unsubscribe is called on all return paths (including early returns).
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // Virt layer: 2-level aggregation —
        //   Inner: dispatch per region (fans out to all servers for that region)
        //   Outer: collect across regions
        let virt_fut = async {
            if !virt_enabled {
                return Vec::new();
            }
            let Some(injection_query) = self.language.injection_query(&language_name) else {
                return Vec::new();
            };
            let all_regions = InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                &uri,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
            );
            if all_regions.is_empty() {
                return Vec::new();
            }

            let mut outer_join_set: JoinSet<Vec<Diagnostic>> = JoinSet::new();
            for resolved in all_regions {
                let configs = self.bridge_configs_for_injection_language(
                    &language_name,
                    &resolved.injection_language,
                );
                if configs.is_empty() {
                    continue;
                }

                // Resolve strategy per-region so different injection languages can use
                // different strategies (e.g., Python=Preferred, Lua=All in the same host).
                let agg = self.resolve_aggregation_config(
                    &language_name,
                    &resolved.injection_language,
                    "textDocument/diagnostic",
                );
                let strategy = agg.strategy;
                let region_ctx = DocumentRequestContext {
                    uri: uri.clone(),
                    resolved,
                    configs,
                    upstream_request_id: upstream_request_id.clone(),
                    priorities: agg.priorities,
                    strategy,
                    max_fan_out: agg.max_fan_out,
                };
                let pool = Arc::clone(&pool);

                outer_join_set
                    .spawn(async move { collect_region_diagnostics(&region_ctx, pool).await });
            }

            // Cancellation is handled by the outer select dropping this
            // future (which drops the JoinSet, aborting region tasks).
            crate::lsp::aggregation::region::collect_region_results_with_cancel(
                outer_join_set,
                None,
                |acc, items: Vec<Diagnostic>| acc.extend(items),
            )
            .await
            .unwrap_or_default()
        };

        // Host layer (host-document-bridge): the host language's own servers
        // answer for the real URI; within-layer combine follows
        // `bridge._self.aggregation` (concatenated by default).
        let host_fut = async {
            match &host_ctx {
                Some(ctx) => collect_host_diagnostics(ctx, Arc::clone(&pool)).await,
                None => Vec::new(),
            }
        };

        // Both layers fan out concurrently; the layer strategy decides the
        // combine. Cancel drops both in-flight layer futures.
        let combined = async { tokio::join!(virt_fut, host_fut) };
        let (virt_items, host_items) = match cancel_rx {
            Some(mut cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => {
                        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());
                        return Err(tower_lsp_server::jsonrpc::Error::request_cancelled());
                    }
                    result = combined => result,
                }
            }
            None => combined.await,
        };

        // Clean up stale upstream registry entries once all layer tasks have
        // completed. On the cancel path above the sweep runs BEFORE the
        // dropped futures unwind — safe because cancel forwarding
        // captures its targets before notifying, and the pool tolerates
        // entries unregistered out of order (see forward_cancel_by
        // _upstream_id_with_notify).
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        Ok(make_diagnostic_report(combine_layer_diagnostics(
            &layer_cfg, virt_items, host_items,
        )))
    }
}

/// Combine per-layer diagnostic results by the cross-layer strategy
/// (cross-layer-aggregation): `concatenated` merges every participating
/// layer's items in `priorities` order; `preferred` returns the first
/// layer with a non-empty result. Native has no diagnostics contributor.
pub(crate) fn combine_layer_diagnostics(
    layer_cfg: &ResolvedLayerConfig,
    virt: Vec<Diagnostic>,
    host: Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    let mut virt = Some(virt);
    let mut host = Some(host);
    let mut merged = Vec::new();
    for layer in &layer_cfg.priorities {
        let items = match layer {
            LayerSource::Virt => virt.take(),
            LayerSource::Host => host.take(),
            LayerSource::Native => None,
        };
        let Some(items) = items else { continue };
        match layer_cfg.strategy {
            AggregationStrategy::Concatenated => merged.extend(items),
            AggregationStrategy::Preferred => {
                if !items.is_empty() {
                    return items;
                }
            }
        }
    }
    merged
}

/// Collect diagnostics from the host layer's servers: a
/// `textDocument/diagnostic` pull with the real URI, combined across host
/// servers per `bridge._self.aggregation` (`ctx.strategy`).
pub(crate) async fn collect_host_diagnostics(
    ctx: &HostRequestContext,
    pool: Arc<LanguageServerPool>,
) -> Vec<Diagnostic> {
    let send = |t: HostFanOutTask| async move {
        let result = tokio::time::timeout(
            DIAGNOSTIC_REQUEST_TIMEOUT,
            t.pool.send_host_raw_request(
                &t.server_name,
                &t.server_config,
                &crate::lsp::bridge::HostDocument {
                    uri: &t.uri,
                    language_id: &t.language_id,
                    text: &t.text,
                },
                "textDocument/diagnostic",
                serde_json::json!({ "textDocument": { "uri": t.uri.as_str() } }),
                t.upstream_id,
                // Same policy as the virt diagnostic path: wait through
                // server initialization — the first didOpen-triggered pull
                // would otherwise hit an Initializing server and silently
                // lose the host layer. The outer timeout bounds the wait.
                crate::lsp::bridge::ConnectionReadiness::WaitReady,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            Err(std::io::Error::other(format!(
                "host diagnostic request timed out for {}",
                t.server_name
            )))
        })?;
        Ok(parse_host_diagnostic_report(result))
    };

    // Cancellation is handled by the caller dropping this future.
    match ctx.strategy {
        AggregationStrategy::Concatenated => {
            match dispatch_host_concatenated(ctx, pool, send, None, Some("kakehashi::diagnostic"))
                .await
            {
                FanInResult::Done(vecs) => vecs.into_iter().flatten().collect(),
                FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
            }
        }
        AggregationStrategy::Preferred => {
            match dispatch_host_preferred(
                ctx,
                pool,
                send,
                |v: &Vec<Diagnostic>| !v.is_empty(),
                None,
            )
            .await
            {
                FanInResult::Done(diagnostics) => diagnostics,
                FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
            }
        }
    }
}

/// Parse a raw `textDocument/diagnostic` result from a host server into its
/// diagnostic items: a `full` report yields its items (`relatedDocuments`
/// entries are dropped — diagnostics for OTHER documents have no place in
/// this document's report); `unchanged` (which a server should not send —
/// we never supply `previousResultId`) and `null` / unparsable results
/// yield nothing.
fn parse_host_diagnostic_report(result: Option<serde_json::Value>) -> Vec<Diagnostic> {
    let Some(value) = result else {
        return Vec::new();
    };
    match serde_json::from_value::<DocumentDiagnosticReport>(value) {
        Ok(DocumentDiagnosticReport::Full(report)) => report.full_document_diagnostic_report.items,
        Ok(DocumentDiagnosticReport::Unchanged(_)) => Vec::new(),
        Err(e) => {
            log::debug!(
                target: "kakehashi::diagnostic",
                "host diagnostic report did not parse: {}",
                e
            );
            Vec::new()
        }
    }
}

/// Send a diagnostic request for a single fan-out task with timeout.
///
/// Shared by both concatenated and preferred dispatch strategies.
async fn send_diagnostic_fan_out_request(t: FanOutTask) -> std::io::Result<Vec<Diagnostic>> {
    let rid = t.region_id.clone();
    tokio::time::timeout(
        DIAGNOSTIC_REQUEST_TIMEOUT,
        t.pool.send_diagnostic_request(
            &t.server_name,
            &t.server_config,
            &t.uri,
            &t.injection_language,
            &t.region_id,
            t.offset,
            &t.virtual_content,
            t.upstream_id,
            None, // No previous_result_id
        ),
    )
    .await
    .unwrap_or_else(|_| {
        Err(std::io::Error::other(format!(
            "diagnostic request timed out for region {rid}"
        )))
    })
}

/// Dispatch diagnostics using the concatenated strategy (merge results from every server).
async fn dispatch_concatenated_diagnostics(
    region_ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
) -> Vec<Diagnostic> {
    let result = dispatch_concatenated(
        region_ctx,
        pool,
        send_diagnostic_fan_out_request,
        None, // cancel handled at outer level
        None, // no custom log_target
    )
    .await;

    match result {
        FanInResult::Done(vecs) => vecs.into_iter().flatten().collect(),
        FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
    }
}

/// Dispatch diagnostics using the preferred strategy (first non-empty response wins).
async fn dispatch_preferred_diagnostics(
    region_ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
) -> Vec<Diagnostic> {
    let result = dispatch_preferred(
        region_ctx,
        pool,
        send_diagnostic_fan_out_request,
        |v: &Vec<Diagnostic>| !v.is_empty(),
        None, // cancel handled at outer level
    )
    .await;

    match result {
        FanInResult::Done(diagnostics) => diagnostics,
        FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
    }
}

/// Create a full diagnostic report from aggregated diagnostics.
fn make_diagnostic_report(diagnostics: Vec<Diagnostic>) -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport {
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None, // No result_id for aggregated multi-region response
                items: diagnostics,
            },
            related_documents: None,
        },
    ))
}

/// Create an empty diagnostic report (full report with no items).
fn empty_diagnostic_report() -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport {
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: None,
                items: Vec::new(),
            },
            related_documents: None,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tower_lsp_server::ls_types::{Position, Range};

    fn diag(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            message: message.to_string(),
            ..Default::default()
        }
    }

    fn layer_cfg(
        priorities: Vec<LayerSource>,
        strategy: AggregationStrategy,
    ) -> ResolvedLayerConfig {
        ResolvedLayerConfig {
            priorities,
            strategy,
        }
    }

    #[test]
    fn combine_concatenated_merges_in_priority_order() {
        let cfg = layer_cfg(
            vec![LayerSource::Host, LayerSource::Virt, LayerSource::Native],
            AggregationStrategy::Concatenated,
        );
        let merged = combine_layer_diagnostics(&cfg, vec![diag("virt")], vec![diag("host")]);
        assert_eq!(
            merged
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>(),
            vec!["host", "virt"],
            "items follow the layer priorities order"
        );
    }

    #[test]
    fn combine_concatenated_skips_omitted_layers() {
        let cfg = layer_cfg(vec![LayerSource::Virt], AggregationStrategy::Concatenated);
        let merged = combine_layer_diagnostics(&cfg, vec![diag("virt")], vec![diag("host")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].message, "virt", "host omitted from priorities");
    }

    #[test]
    fn combine_preferred_first_nonempty_layer_wins() {
        let cfg = layer_cfg(
            vec![LayerSource::Virt, LayerSource::Host],
            AggregationStrategy::Preferred,
        );
        // Virt empty -> host wins
        let merged = combine_layer_diagnostics(&cfg, vec![], vec![diag("host")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].message, "host");
        // Virt non-empty -> virt wins, host dropped
        let merged = combine_layer_diagnostics(&cfg, vec![diag("virt")], vec![diag("host")]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].message, "virt");
    }

    #[test]
    fn combine_empty_priorities_yields_nothing() {
        let cfg = layer_cfg(vec![], AggregationStrategy::Concatenated);
        assert!(combine_layer_diagnostics(&cfg, vec![diag("virt")], vec![diag("host")]).is_empty());
    }

    #[test]
    fn parse_host_report_full_yields_items() {
        let value = json!({
            "kind": "full",
            "items": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
                "message": "host says hi"
            }]
        });
        let items = parse_host_diagnostic_report(Some(value));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].message, "host says hi");
    }

    #[test]
    fn parse_host_report_unchanged_none_and_garbage_yield_nothing() {
        assert!(parse_host_diagnostic_report(None).is_empty());
        assert!(
            parse_host_diagnostic_report(Some(json!({"kind": "unchanged", "resultId": "x"})))
                .is_empty()
        );
        assert!(parse_host_diagnostic_report(Some(json!("nonsense"))).is_empty());
        assert!(parse_host_diagnostic_report(Some(serde_json::Value::Null)).is_empty());
    }
}
