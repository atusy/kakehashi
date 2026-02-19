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
use url::Url;

use super::super::{Kakehashi, uri_to_url};
use crate::config::settings::{AggregationStrategy, BridgeServerConfig};
use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, dispatch_collect_all, dispatch_preferred,
};
use crate::lsp::bridge::{LanguageServerPool, UpstreamId};
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;

// ============================================================================
// Shared diagnostic utilities (used by both pull and synthetic push diagnostics)
// ============================================================================

/// Per-request timeout for diagnostic fan-out (ADR-0020).
///
/// Used by both pull diagnostics (textDocument/diagnostic) and
/// synthetic push diagnostics (didSave/didOpen/didChange triggered).
pub(crate) const DIAGNOSTIC_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Information needed to send a diagnostic request for one injection region.
///
/// This struct captures all the data required to make a single diagnostic request
/// to a downstream language server. It's used during parallel fan-out where
/// multiple injection regions are processed concurrently.
///
/// Uses Arc for config to avoid cloning large structs for each region - multiple
/// regions using the same language server share the same config Arc.
pub(crate) struct DiagnosticRequestInfo {
    /// Name of the downstream language server (e.g., "lua-language-server")
    pub(crate) server_name: String,
    /// Shared reference to the bridge server configuration
    pub(crate) config: Arc<BridgeServerConfig>,
    /// Language of the injection region (e.g., "lua", "python")
    pub(crate) injection_language: String,
    /// Unique identifier for this injection region within the host document
    pub(crate) region_id: String,
    /// Starting line of the injection region in the host document (0-indexed)
    /// Used to transform diagnostic positions back to host coordinates
    pub(crate) region_start_line: u32,
    /// The extracted content of the injection region, formatted as a virtual document
    /// This is what gets sent to the downstream language server for analysis
    pub(crate) virtual_content: String,
}

/// Send a diagnostic request with timeout, returning parsed diagnostics or None on failure.
///
/// This is the shared implementation used by both pull and push diagnostics.
/// It handles timeout, error logging, and response parsing.
///
/// # Arguments
/// * `pool` - The language server pool for sending requests
/// * `info` - Request info containing server details and region data
/// * `uri` - The host document URI
/// * `upstream_request_id` - The upstream request ID for cancel forwarding
/// * `previous_result_id` - Optional result ID for unchanged detection
/// * `timeout` - Request timeout duration
/// * `log_target` - Logging target (e.g., "kakehashi::diagnostic")
pub(crate) async fn send_diagnostic_with_timeout(
    pool: &LanguageServerPool,
    info: &DiagnosticRequestInfo,
    uri: &Url,
    upstream_request_id: Option<UpstreamId>,
    previous_result_id: Option<&str>,
    timeout: Duration,
    log_target: &str,
) -> Option<Vec<Diagnostic>> {
    let request_future = pool.send_diagnostic_request(
        &info.server_name,
        &info.config,
        uri,
        &info.injection_language,
        &info.region_id,
        info.region_start_line,
        &info.virtual_content,
        upstream_request_id,
        previous_result_id,
    );

    // Apply timeout per-request (ADR-0020: return partial results on timeout)
    match tokio::time::timeout(timeout, request_future).await {
        Ok(Ok(diagnostics)) => Some(diagnostics),
        Ok(Err(e)) => {
            log::warn!(
                target: log_target,
                "Diagnostic request failed for region {}: {}",
                info.region_id,
                e
            );
            None
        }
        Err(_) => {
            log::warn!(
                target: log_target,
                "Diagnostic request timed out for region {} after {:?}",
                info.region_id,
                timeout
            );
            None
        }
    }
}

/// Fan-out diagnostic requests to downstream servers.
///
/// Spawns parallel requests to all injection regions and aggregates results.
/// Uses JoinSet for structured concurrency with automatic cleanup on drop.
///
/// This is the shared implementation used by both synthetic push diagnostics
/// (didSave/didOpen triggered) and debounced diagnostics (didChange triggered).
///
/// # Arguments
/// * `pool` - The language server pool for sending requests
/// * `uri` - The host document URI
/// * `request_infos` - Request info for each injection region
/// * `log_target` - Logging target (e.g., "kakehashi::synthetic_diag")
pub(crate) async fn fan_out_diagnostic_requests(
    pool: &Arc<LanguageServerPool>,
    uri: &Url,
    request_infos: Vec<DiagnosticRequestInfo>,
    log_target: &'static str,
) -> Vec<Diagnostic> {
    let mut join_set = tokio::task::JoinSet::new();

    for info in request_infos {
        let pool = Arc::clone(pool);
        let uri = uri.clone();

        join_set.spawn(async move {
            send_diagnostic_with_timeout(
                &pool,
                &info,
                &uri,
                None, // No upstream request for background tasks
                None, // No previous_result_id
                DIAGNOSTIC_REQUEST_TIMEOUT,
                log_target,
            )
            .await
        });
    }

    // Collect results from all regions
    let mut all_diagnostics: Vec<Diagnostic> = Vec::new();
    while let Some(result) = join_set.join_next().await {
        match result {
            Ok(Some(diagnostics)) => all_diagnostics.extend(diagnostics),
            Ok(None) => {}
            Err(e) => {
                log::error!(
                    target: log_target,
                    "Diagnostic task panicked: {}",
                    e
                );
            }
        }
    }

    all_diagnostics
}

// ============================================================================
// Pull diagnostics implementation (textDocument/diagnostic)
// ============================================================================

impl Kakehashi {
    /// Build DiagnosticRequestInfo for all injection regions that have bridge configs.
    ///
    /// Groups by server_name to share Arc<BridgeServerConfig> across regions using the same server.
    ///
    /// This is `pub(super)` for use by both pull diagnostics and synthetic push diagnostics.
    pub(super) fn build_diagnostic_request_infos(
        &self,
        language_name: &str,
        all_regions: &[crate::language::injection::ResolvedInjection],
    ) -> Vec<DiagnosticRequestInfo> {
        // Pre-allocate with small capacity (typically 1-2 unique servers per document)
        let mut config_cache: std::collections::HashMap<String, Arc<BridgeServerConfig>> =
            std::collections::HashMap::with_capacity(2);
        let mut request_infos: Vec<DiagnosticRequestInfo> = Vec::with_capacity(all_regions.len());

        for resolved in all_regions {
            // Get ALL bridge server configs for this language (N-server fan-out).
            // For each region, we emit one DiagnosticRequestInfo per matching server,
            // enabling diagnostics from multiple servers (e.g., pyright + ruff for Python).
            let configs = self
                .get_all_bridge_configs_for_language(language_name, &resolved.injection_language);

            if configs.is_empty() {
                log::debug!(
                    target: "kakehashi::diagnostic",
                    "No bridge config for language {}",
                    resolved.injection_language
                );
                continue;
            }

            for resolved_config in configs {
                let server_name = resolved_config.server_name.clone();

                // Reuse Arc if we've already seen this server, otherwise clone the existing Arc.
                // Since ResolvedServerConfig.config is already Arc-wrapped, this is cheap.
                let config_arc = config_cache
                    .entry(server_name.clone())
                    .or_insert_with(|| Arc::clone(&resolved_config.config))
                    .clone();

                request_infos.push(DiagnosticRequestInfo {
                    server_name,
                    config: config_arc,
                    injection_language: resolved.injection_language.clone(),
                    region_id: resolved.region.region_id.clone(),
                    region_start_line: resolved.region.line_range.start,
                    virtual_content: resolved.virtual_content.clone(),
                });
            }
        }

        request_infos
    }

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

        // Resolve strategy once — all regions share the same host language config.
        // Default is All (collect from every server), preserving existing behavior.
        let strategy = self.resolve_aggregation_strategy(
            &language_name,
            // Use first region's injection language for strategy resolution.
            // In practice, diagnostic regions from the same host share config.
            &all_regions[0].injection_language,
            "textDocument/diagnostic",
            AggregationStrategy::All,
        );

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
