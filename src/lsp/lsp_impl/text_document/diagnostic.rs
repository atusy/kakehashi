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
    FullDocumentDiagnosticReport, MessageType, RelatedFullDocumentDiagnosticReport,
};

use super::super::{Kakehashi, uri_to_url};
use crate::config::settings::AggregationStrategy;
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, dispatch_collect_all, dispatch_preferred,
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

/// Collect diagnostics for a single injection region using priority-aware aggregation.
///
/// Wraps `dispatch_collect_all` with the standard timeout closure and
/// `unregister_all_for_upstream_id` cleanup. Used by push diagnostic
/// helpers in `publish_diagnostic.rs` (`spawn_synthetic_diagnostic_task`
/// and `execute_debounced_diagnostic`).
///
/// Pull diagnostics (`diagnostic_impl`) use `dispatch_collect_all_diagnostics`
/// and `dispatch_preferred_diagnostics` directly to support per-region
/// strategy selection.
pub(crate) async fn collect_region_diagnostics(
    ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    log_target: Option<&str>,
) -> Vec<Diagnostic> {
    let result = dispatch_collect_all(
        ctx,
        pool.clone(),
        send_diagnostic_fan_out_request,
        None, // cancel handled at outer level (pull) or not needed (push)
        log_target,
    )
    .await;

    // Clean up stale upstream registry entries from inner fan_out
    pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());

    match result {
        FanInResult::Done(vecs) => vecs.into_iter().flatten().collect(),
        FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
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

        // Use LOG level (lowest severity) for per-request logging in hot path
        // to avoid flooding client with INFO messages on frequent diagnostic requests
        self.client
            .log_message(
                MessageType::LOG,
                format!("textDocument/diagnostic called for {}", uri),
            )
            .await;

        // Get document snapshot (minimizes lock duration)
        let (snapshot, missing_message) = match self.documents.get(&uri) {
            None => (None, Some("No document found")),
            Some(doc) => match doc.snapshot() {
                None => (None, Some("Document not fully initialized")),
                Some(snapshot) => (Some(snapshot), None),
            },
            // doc automatically dropped here, lock released
        };
        if let Some(message) = missing_message {
            self.client.log_message(MessageType::INFO, message).await;
            return Ok(empty_diagnostic_report());
        }
        let snapshot = snapshot.expect("snapshot set when missing_message is None");

        // Get the language for this document
        let Some(language_name) = self.get_language_for_document(&uri) else {
            log::debug!(target: "kakehashi::diagnostic", "No language detected");
            return Ok(empty_diagnostic_report());
        };

        // Get injection query to detect injection regions
        let Some(injection_query) = self.language.get_injection_query(&language_name) else {
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
        let upstream_request_id = super::super::bridge_context::current_upstream_id();

        // Subscribe to cancel notifications for this request.
        // The guard ensures unsubscribe is called on all return paths (including early returns).
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // 2-level aggregation:
        //   Inner: dispatch per region (fans out to all servers for that region)
        //   Outer: collect_region_results_with_cancel across regions
        let mut outer_join_set: JoinSet<Vec<Diagnostic>> = JoinSet::new();

        for resolved in all_regions {
            let configs = self
                .get_all_bridge_configs_for_language(&language_name, &resolved.injection_language);
            if configs.is_empty() {
                continue;
            }

            // Resolve strategy per-region so different injection languages can use
            // different strategies (e.g., Python=Preferred, Lua=All in the same host).
            let strategy = self.resolve_aggregation_strategy(
                &language_name,
                &resolved.injection_language,
                "textDocument/diagnostic",
                AggregationStrategy::All,
            );
            let priorities = self.resolve_aggregation_priorities(
                &language_name,
                &resolved.injection_language,
                "textDocument/diagnostic",
            );
            let region_ctx = DocumentRequestContext {
                uri: uri.clone(),
                resolved,
                configs,
                upstream_request_id: upstream_request_id.clone(),
                priorities,
            };
            let pool = Arc::clone(&pool);

            outer_join_set.spawn(async move {
                let diagnostics = match strategy {
                    AggregationStrategy::All => {
                        dispatch_collect_all_diagnostics(&region_ctx, pool.clone()).await
                    }
                    AggregationStrategy::Preferred => {
                        dispatch_preferred_diagnostics(&region_ctx, pool.clone()).await
                    }
                };

                // Clean up stale upstream registry entries from inner fan_out
                pool.unregister_all_for_upstream_id(region_ctx.upstream_request_id.as_ref());

                diagnostics
            });
        }

        // Collect results from all regions, aborting early if $/cancelRequest arrives.
        let result = crate::lsp::aggregation::region::collect_region_results_with_cancel(
            outer_join_set,
            cancel_rx,
            |acc, items: Vec<Diagnostic>| acc.extend(items),
        )
        .await;

        let all_diagnostics = result?;

        Ok(make_diagnostic_report(all_diagnostics))
    }
}

/// Send a diagnostic request for a single fan-out task with timeout.
///
/// Shared by both collect-all and preferred dispatch strategies.
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
            t.region_start_line,
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

/// Dispatch diagnostics using the collect-all strategy (merge results from every server).
async fn dispatch_collect_all_diagnostics(
    region_ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
) -> Vec<Diagnostic> {
    let result = dispatch_collect_all(
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
