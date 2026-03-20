//! Pull diagnostics for Kakehashi (textDocument/diagnostic).
//!
//! Implements ADR-0020 Phase 1: Pull-first diagnostic forwarding.
//! Sprint 17: Multi-region diagnostic aggregation with parallel fan-out.
//!
//! For synthetic push diagnostics (publishDiagnostics), see `publish_diagnostic.rs`.
//!
//! # Cancel Handling
//!
//! This module supports immediate cancellation of diagnostic requests:
//! - When `$/cancelRequest` is received, the handler aborts and returns `RequestCancelled`
//! - The JoinSet is dropped, aborting all spawned downstream tasks
//! - Best-effort cancel forwarding to downstream servers (fire-and-forget via middleware)
//!
//! This is achieved using `tokio::select!` to race between:
//! 1. Cancel notification (via `CancelForwarder::subscribe()`)
//! 2. Result aggregation (collecting from all downstream tasks)

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport,
};

use super::super::{Kakehashi, uri_to_url};
use crate::config::settings::AggregationStrategy;
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, dispatch_concatenated, dispatch_preferred,
};
use crate::lsp::bridge::LanguageServerPool;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

// ============================================================================
// Shared diagnostic utilities (used by both pull and push diagnostics)
// ============================================================================

/// Per-request timeout for diagnostic fan-out (ADR-0020).
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

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(empty_diagnostic_report());
        };

        // Collect all injection regions
        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.region_id_tracker(),
            &uri,
            snapshot.tree(),
            snapshot.text(),
            injection_query.as_ref(),
        );

        if all_regions.is_empty() {
            return Ok(empty_diagnostic_report());
        }

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = crate::lsp::current_upstream_id();

        // Subscribe to cancel notifications for this request.
        // The guard ensures unsubscribe is called on all return paths (including early returns).
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // 2-level aggregation:
        //   Inner: dispatch per region (fans out to all servers for that region)
        //   Outer: collect_region_results_with_cancel across regions
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
                AggregationStrategy::Concatenated,
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

            outer_join_set.spawn(async move {
                match strategy {
                    AggregationStrategy::Concatenated => {
                        dispatch_concatenated_diagnostics(&region_ctx, pool.clone()).await
                    }
                    AggregationStrategy::Preferred => {
                        dispatch_preferred_diagnostics(&region_ctx, pool.clone()).await
                    }
                }
            });
        }

        // Collect results from all regions, aborting early if $/cancelRequest arrives.
        let result = crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_rx,
            |acc, items: Vec<Diagnostic>| acc.extend(items),
        )
        .await;

        // Clean up stale upstream registry entries once all region tasks have completed
        // (or been aborted via JoinSet drop). Must happen after the JoinSet is drained
        // so cancel forwarding remains intact for all in-flight downstream requests.
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());

        let concatenated_diagnostics = result?;

        Ok(make_diagnostic_report(concatenated_diagnostics))
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
