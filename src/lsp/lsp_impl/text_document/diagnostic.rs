//! `textDocument/diagnostic` (pull): Phase 1 of pull-first-diagnostic-forwarding,
//! with multi-region aggregation via parallel fan-out. Push side lives in
//! `publish_diagnostic.rs`.
//!
//! `tokio::select!` races `CancelForwarder::subscribe()` against result
//! aggregation: a `$/cancelRequest` returns `RequestCancelled`, dropping the
//! `JoinSet` aborts all downstream tasks, and cancels are also forwarded
//! downstream fire-and-forget via middleware.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
};
use url::Url;

use super::super::{Kakehashi, uri_to_url};
use crate::config::settings::{AggregationStrategy, LayerSource, ResolvedLayerConfig};
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, HostFanOutTask, dispatch_concatenated, dispatch_host_concatenated,
    dispatch_host_preferred, dispatch_preferred,
};
use crate::lsp::bridge::{LanguageServerPool, RegionOffset};
use crate::lsp::diagnostic_cache::{DiagnosticSource, cached_push_diagnostics, push_slot_servers};
use crate::lsp::lsp_impl::bridge_context::{
    DocumentRequestContext, HostRequestContext, resolve_aggregation_config_from_settings,
};
use crate::lsp::lsp_impl::text_document::{RequestErrorSink, count_request_errors};

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
    request_error_sink: &RequestErrorSink,
) -> Vec<Diagnostic> {
    match ctx.strategy {
        AggregationStrategy::Concatenated => {
            dispatch_concatenated_diagnostics(ctx, pool, request_error_sink).await
        }
        AggregationStrategy::Preferred => {
            dispatch_preferred_diagnostics(ctx, pool, request_error_sink).await
        }
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
        self.diagnostic_impl_with_error_sink(params, None).await
    }

    /// Like [`Self::diagnostic_impl`], but records request-time downstream
    /// failures — timeouts, crashes, or error responses that occur AFTER a
    /// server is ready — into `request_error_sink`. LSP mode passes `None`
    /// (failures are log-only; the editor retries); CLI `diagnose` passes
    /// `Some` so a one-shot run can map them onto exit 2 instead of a false
    /// "clean". Startup failures are detected separately by the CLI ready-wait.
    pub(crate) async fn diagnostic_impl_with_error_sink(
        &self,
        params: DocumentDiagnosticParams,
        request_error_sink: RequestErrorSink,
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

        // Resolve injection regions once: the live virt pull below and the
        // pushFallback fold (#425) after the join share them.
        let virt_regions = if virt_enabled {
            self.language
                .injection_query(&language_name)
                .map(|injection_query| {
                    InjectionResolver::resolve_all(
                        &self.language,
                        self.bridge.node_tracker(),
                        &uri,
                        snapshot.tree(),
                        snapshot.text(),
                        injection_query.as_ref(),
                    )
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // Lightweight per-region metadata `(region_id, injection_language,
        // current offset)` for the pushFallback fold; the live pull below moves
        // the regions themselves.
        let region_meta: Vec<(String, String, RegionOffset)> = virt_regions
            .iter()
            .map(|r| {
                (
                    r.region.region_id.clone(),
                    r.injection_language.clone(),
                    RegionOffset::with_per_line_offsets(
                        r.region.line_range.start,
                        r.line_column_offsets.clone(),
                    ),
                )
            })
            .collect();

        // Virt layer: 2-level aggregation —
        //   Inner: dispatch per region (fans out to all servers for that region)
        //   Outer: collect across regions
        let virt_fut = async {
            let mut outer_join_set: JoinSet<Vec<Diagnostic>> = JoinSet::new();
            for resolved in virt_regions {
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
                    client_progress_token: None,
                };
                let pool = Arc::clone(&pool);
                let task_sink = request_error_sink.clone();

                outer_join_set.spawn(async move {
                    collect_region_diagnostics(&region_ctx, pool, &task_sink).await
                });
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
                Some(ctx) => {
                    collect_host_diagnostics(ctx, Arc::clone(&pool), &request_error_sink).await
                }
                None => Vec::new(),
            }
        };

        // Both layers fan out concurrently; the layer strategy decides the
        // combine. Cancel drops both in-flight layer futures.
        let combined = async { tokio::join!(virt_fut, host_fut) };
        let (mut virt_items, mut host_items) = match cancel_rx {
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

        // pushFallback (Path B, #425): the live pulls above cover only
        // pull-driven servers (capability-gated). Fold in push-driven servers'
        // cached pushes so they also answer the client pull.
        self.fold_push_fallback_diagnostics(
            &uri,
            &language_name,
            region_meta,
            host_ctx.is_some(),
            &mut virt_items,
            &mut host_items,
        )
        .await;

        Ok(make_diagnostic_report(combine_layer_diagnostics(
            &layer_cfg, virt_items, host_items,
        )))
    }

    /// pushFallback fold (Path B, #425): append push-driven servers' cached
    /// pushes to the per-layer pull results. The live pull already covers
    /// pull-driven servers (their native source), so only servers that do **not**
    /// advertise `diagnosticProvider` are folded — their cached pushes are their
    /// native source. Region pushes are transformed to host coordinates against
    /// the region's current offset; host pushes are already host-local.
    ///
    /// `host_layer_participates` is `host_ctx.is_some()` — the host layer is in
    /// the method's priorities AND `bridge._self` is opted in with configured
    /// servers (capability is *not* required: a push-only `_self` server yields a
    /// host context whose live pull returns empty, and this fold supplies it).
    ///
    /// Under a per-region `strategy = preferred`, the folded push-driven slots are
    /// *appended* after the region's live election rather than competing in it —
    /// consistent with Path A's concatenate-everything merge and the deferred
    /// per-source strategy fan-in (push-propagation-diagnostic-forwarding), not an
    /// election bug. For the same reason the fold honors only `pushFallback`, not
    /// the visible walk: it does not re-apply `priorities`/`maxFanOut`, so a
    /// push-driven server outside the walk is still folded — exactly what Path A's
    /// proactive merge already publishes, until the deferred fan-in resolves the
    /// walk for both paths together.
    async fn fold_push_fallback_diagnostics(
        &self,
        host: &Url,
        language_name: &str,
        region_meta: Vec<(String, String, RegionOffset)>,
        host_layer_participates: bool,
        virt_items: &mut Vec<Diagnostic>,
        host_items: &mut Vec<Diagnostic>,
    ) {
        // One snapshot drives both the candidate classification and the fold, so
        // a push arriving across the classifying `await` below cannot land in the
        // folded set while skipping classification (no TOCTOU double-count).
        let snapshot = self.diagnostics.snapshot(host);
        let candidates = push_slot_servers(&snapshot);
        if candidates.is_empty() {
            return; // no cached pushes for this host
        }
        // Classify live (static caps + dynamic registrations); fold only the
        // push-driven ones so a pull-driven server is not double-counted.
        let pull_driven = self.bridge.pool().pull_driven_servers(&candidates).await;

        // Load settings once: resolving per-region pushFallback in the loop and
        // the host gate below otherwise re-reads it N+1 times and could resolve
        // different regions against different snapshots if config is swapped
        // mid-request.
        let settings = self.settings_manager.load_settings();

        // Per-region `pushFallback` gate + current offsets for the transform.
        let mut region_offsets = HashMap::new();
        let mut region_push_enabled = HashSet::new();
        for (region_id, injection_language, offset) in region_meta {
            if resolve_aggregation_config_from_settings(
                &settings,
                language_name,
                &injection_language,
                "textDocument/diagnostic",
            )
            .push_fallback
            {
                region_push_enabled.insert(region_id.clone());
            }
            // `region_meta` is consumed: move the id + offset (a heap `Vec`) in
            // rather than cloning per region.
            region_offsets.insert(region_id, offset);
        }

        // Host `pushFallback` gate: the host layer participates AND pushFallback
        // is on for the host's diagnostic method.
        let host_push_enabled = host_layer_participates
            && settings
                .resolve_host_language_settings(language_name)
                .map(|s| {
                    s.resolve_host_aggregation("textDocument/diagnostic")
                        .push_fallback
                })
                .unwrap_or(false);

        let include = |source: &DiagnosticSource, server: &str| {
            // Pull-driven servers are excluded unconditionally: their native
            // source is the live pull above, which already gates on the same
            // capability AND on this method's `priorities`/`maxFanOut`. A
            // pull-driven server that `priorities` drops from the live fan-out is
            // therefore also dropped here — config-consistent, not a leak. (The
            // only lossy corner is a pull-driven server whose live pull times out
            // this cycle while holding a stale cached push; transient, and it
            // never hides a current diagnostic.)
            if pull_driven.contains(server) {
                return false; // covered by the live pull
            }
            match source {
                DiagnosticSource::Region(id) => region_push_enabled.contains(id),
                DiagnosticSource::Host => host_push_enabled,
                DiagnosticSource::PullLayer => false,
            }
        };
        let (region_push, host_push) =
            cached_push_diagnostics(host, snapshot, &region_offsets, include);
        virt_items.extend(region_push);
        host_items.extend(host_push);
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
    request_error_sink: &RequestErrorSink,
) -> Vec<Diagnostic> {
    let send = |t: HostFanOutTask| {
        // Cloned per call so the returned future is `'static`, and so a
        // request-time host failure (timeout, crash, error response after the
        // server is ready) is counted — letting CLI mode surface it as exit 2
        // instead of silently losing the host layer.
        let sink = request_error_sink.clone();
        async move {
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
            });
            match result {
                Ok(value) => Ok(parse_host_diagnostic_report(value)),
                Err(e) => {
                    count_request_errors(&sink, 1);
                    Err(e)
                }
            }
        }
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

/// Send one fan-out diagnostic request, counting a request-time failure into
/// `sink`. The fan-in's own `NoResult { errors }` count is discarded once any
/// server succeeds (a partial failure still yields `Done`), so counting here —
/// once per observed failure — is what lets CLI mode surface a downstream
/// server that crashed or timed out after becoming ready (mirrors the
/// formatting path).
/// `sink` is taken by value (an owned `Option<Arc<…>>`) so the returned future
/// is `'static`, as the fan-out dispatch requires.
async fn send_diagnostic_request_counting_errors(
    t: FanOutTask,
    sink: RequestErrorSink,
) -> std::io::Result<Vec<Diagnostic>> {
    let result = send_diagnostic_fan_out_request(t).await;
    if result.is_err() {
        count_request_errors(&sink, 1);
    }
    result
}

/// Dispatch diagnostics using the concatenated strategy (merge results from every server).
async fn dispatch_concatenated_diagnostics(
    region_ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    sink: &RequestErrorSink,
) -> Vec<Diagnostic> {
    let result = dispatch_concatenated(
        region_ctx,
        pool,
        |t| send_diagnostic_request_counting_errors(t, sink.clone()),
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
///
/// KNOWN LIMITATION (CLI exit codes, tracked in #487): when a winner is
/// decided, the preferred fan-in (`aggregation::server::fan_in::preferred`)
/// `abort_all()`s the losing tasks WITHOUT joining them, so a
/// losing server that fails its request can run `count_request_errors` after
/// this returns and after the CLI has loaded `sink`. Such a non-winning
/// failure may therefore not surface as `kakehashi diagnose` exit 2. The
/// default `concatenated` strategy drains every task (`join_next` to `None`)
/// and is exact; the all-servers-fail case yields `NoResult` with no winner,
/// hence no abort and a deterministic count. Only "preferred + a winner + a
/// non-winning failure" is racy — and under preferred semantics the winner's
/// result is authoritative, so whether that should be exit 2 at all is a
/// product question. See #487 for the fix options (shared with the formatting
/// preferred path).
async fn dispatch_preferred_diagnostics(
    region_ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    sink: &RequestErrorSink,
) -> Vec<Diagnostic> {
    let result = dispatch_preferred(
        region_ctx,
        pool,
        |t| send_diagnostic_request_counting_errors(t, sink.clone()),
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
