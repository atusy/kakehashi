//! `textDocument/diagnostic` (pull): Phase 1 of pull-first-diagnostic-forwarding,
//! with multi-region aggregation via parallel fan-out. Push side lives in
//! `publish_diagnostic.rs`.
//!
//! `tokio::select!` races `CancelForwarder::subscribe()` against result
//! aggregation: a `$/cancelRequest` returns `RequestCancelled`, dropping the
//! `JoinSet` aborts all downstream tasks, and cancels are also forwarded
//! downstream fire-and-forget via middleware.

use std::collections::HashMap;
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
use crate::lsp::lsp_impl::text_document::{
    RequestErrorSink, count_no_winner_errors, count_request_errors,
};

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

        // A missing document means a `didClose` removed it — nothing to report.
        if self.documents.get(&uri).is_none() {
            log::debug!("textDocument/diagnostic: No document found for {}", uri);
            return Ok(empty_diagnostic_report());
        }

        // Capture the coverage version BEFORE doing any work (the fold below reads
        // the push cache), so the `served` we record once we hand the editor a report
        // is a lower bound — never ahead of the set it actually receives (#497). A
        // concurrent push bumping `current` after this read just makes the *next*
        // refresh redundant, never skips a needed one. Recorded only on the paths
        // that return a report for this open doc (not the cancel path).
        //
        // Known limitation (deferred epoch class, like `did_close.rs`): if this doc
        // is closed and re-opened while this pull is in flight, `forget_coverage`
        // resets the entry to 0 and this captured version becomes stale-HIGH for the
        // re-opened incarnation. `mark_served` then sets `served` above the re-opened
        // `current`, leaving the gate briefly stuck-clean → a needed refresh can be
        // skipped (narrow staleness) until `current` catches back up. Closing this
        // fully needs a per-incarnation epoch; the editor's own re-open pull
        // self-heals it.
        let served_version = self.diagnostics.current_version(&uri);

        // Get the language for this document
        let Some(language_name) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::diagnostic", "No language detected");
            self.diagnostics.mark_served(&uri, served_version);
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
            self.diagnostics.mark_served(&uri, served_version);
            return Ok(empty_diagnostic_report());
        }

        // Snapshot for the VIRT layer ONLY, and ONLY ensure a fresh tree when virt
        // actually participates: `didChange` clears the tree and reparses
        // off-ingress, so the virt injection regions would otherwise be empty for
        // the reparse window after each edit. The HOST layer needs no tree, so a
        // host-only document must not pay the freshness wait. A still-missing tree
        // (parse pending/failed) yields `None` — host still pulls, virt skips.
        let snapshot = if virt_enabled {
            self.ensure_document_parsed(&uri).await;
            self.documents.get(&uri).and_then(|doc| doc.snapshot())
        } else {
            None
        };

        // Get upstream request ID from task-local storage (set by RequestIdCapture middleware)
        let upstream_request_id = crate::lsp::current_upstream_id();

        // Subscribe to cancel notifications for this request.
        // The guard ensures unsubscribe is called on all return paths (including early returns).
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_request_id.as_ref());

        let pool = self.bridge.pool_arc();

        // Resolve injection regions once: the live virt pull below and the
        // pushFallback fold (#425) after the join share them.
        let virt_regions = match (virt_enabled, snapshot.as_ref()) {
            (true, Some(snapshot)) => self
                .language
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
                .unwrap_or_default(),
            // Virt gated off, or no tree yet (the host layer still pulls). The
            // wait+on-demand above already tried, so a missing tree here is the
            // rare parse-failure case, self-healing on the next pull.
            _ => Vec::new(),
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

        // The editor is about to receive the current merged set: advance `served` to
        // the version captured at entry, so the refresh gate stops treating those
        // changes as dirty (#497). Pure bookkeeping — never republishes — so this
        // can't beget a refresh.
        self.diagnostics.mark_served(&uri, served_version);

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

        // Current offsets for the transform, keyed by region — built ONLY for
        // regions whose `pushFallback` is on (resolved once per distinct
        // injection language, since a document can hold many regions of one
        // language). A region without an offset is skipped by
        // `cached_push_diagnostics`, so this map doubles as the per-region
        // pushFallback gate — no separate set, no `region_id` clone.
        let mut region_offsets = HashMap::new();
        let mut push_fallback_by_lang: HashMap<String, bool> = HashMap::new();
        for (region_id, injection_language, offset) in region_meta {
            // `get` on the common (cache-hit) path is a single lookup; only the
            // resolving miss touches the map again, moving the owned language key
            // in (no clone).
            let push_fallback = match push_fallback_by_lang.get(&injection_language) {
                Some(&fallback) => fallback,
                None => {
                    let fallback = resolve_aggregation_config_from_settings(
                        &settings,
                        language_name,
                        &injection_language,
                        "textDocument/diagnostic",
                    )
                    .push_fallback;
                    push_fallback_by_lang.insert(injection_language, fallback);
                    fallback
                }
            };
            if push_fallback {
                region_offsets.insert(region_id, offset);
            }
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
                // A `Region` slot only reaches `include` when `region_offsets`
                // has its offset, i.e. its `pushFallback` is on — so the gate is
                // already applied; nothing more to check beyond `pull_driven`.
                DiagnosticSource::Region(_) => true,
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
    // Cancellation is handled by the caller dropping this future.
    match ctx.strategy {
        AggregationStrategy::Concatenated => {
            // Concatenated merges every layer, so a crashed/timed-out server is
            // a missing contribution — count each failure in-task. A partial
            // failure still yields `Done` (which discards the fan-in's own
            // count), and the drain is exhaustive, so in-task counting is both
            // necessary and deterministic here.
            let send = |t: HostFanOutTask| {
                let sink = request_error_sink.clone();
                async move {
                    let result = send_host_diagnostic_fan_out_request(t).await;
                    if result.is_err() {
                        count_request_errors(&sink, 1);
                    }
                    result
                }
            };
            match dispatch_host_concatenated(
                ctx,
                pool,
                send,
                None,
                Some("kakehashi::diagnostic"),
                // Surface panic failures (the in-task `send` counts I/O errors
                // only, and `Done` drops the fan-in's own tally) (#506).
                request_error_sink.as_deref(),
            )
            .await
            {
                FanInResult::Done(vecs) => vecs.into_iter().flatten().collect(),
                FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
            }
        }
        AggregationStrategy::Preferred => {
            // Preferred: the winning server is authoritative, so a non-winning
            // server's failure is irrelevant and must NOT be counted (counting
            // it in-task is also racy — the fan-in aborts losers without joining
            // them). Count only when no server won, from the fan-in result
            // (#487).
            let result = dispatch_host_preferred(
                ctx,
                pool,
                send_host_diagnostic_fan_out_request,
                |v: &Vec<Diagnostic>| !v.is_empty(),
                None,
            )
            .await;
            count_no_winner_errors(&result, request_error_sink);
            match result {
                FanInResult::Done(diagnostics) => diagnostics,
                FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
            }
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

/// Send a host diagnostic request for a single fan-out task with timeout, with
/// **no** error counting. The caller counts per strategy: concatenated counts
/// every failure in-task (a missing merge contribution); preferred counts only
/// the no-winner case via [`count_no_winner_errors`] (#487).
async fn send_host_diagnostic_fan_out_request(
    t: HostFanOutTask,
) -> std::io::Result<Vec<Diagnostic>> {
    tokio::time::timeout(
        DIAGNOSTIC_REQUEST_TIMEOUT,
        t.pool.send_host_diagnostic_request(
            &t.server_name,
            &t.server_config,
            &crate::lsp::bridge::HostDocument {
                uri: &t.uri,
                language_id: &t.language_id,
                text: &t.text,
            },
            serde_json::json!({ "textDocument": { "uri": t.uri.as_str() } }),
            t.upstream_id,
        ),
    )
    .await
    .unwrap_or_else(|_| {
        Err(std::io::Error::other(format!(
            "host diagnostic request timed out for {}",
            t.server_name
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
        // Panics unwind before the in-task counting above runs, and a
        // partial-success `Done` discards the fan-in's own tally, so surface
        // panic failures through the sink to keep exit 2 honest (#506).
        sink.as_deref(),
    )
    .await;

    match result {
        FanInResult::Done(vecs) => vecs.into_iter().flatten().collect(),
        FanInResult::NoResult { .. } | FanInResult::Cancelled => Vec::new(),
    }
}

/// Dispatch diagnostics using the preferred strategy (first non-empty response wins).
///
/// Under preferred the winning server's result is authoritative, so a
/// non-winning server's request failure is **not** counted toward CLI exit 2:
/// failures are recorded only when no server won, via [`count_no_winner_errors`]
/// on the fan-in result. This is deterministic — `NoResult` (no winner emerged)
/// drains every task, so its `errors` is the exhaustive count of the actual
/// failures (zero if the non-winners merely returned empty), whereas the racy
/// in-task counting it replaces could miss or catch a loser depending on when
/// the fan-in `abort_all`ed it (#487). The default `concatenated` strategy
/// still counts every failure in-task (each is a missing merge contribution).
/// The same fix applies to the formatting preferred path (tracked separately).
async fn dispatch_preferred_diagnostics(
    region_ctx: &DocumentRequestContext,
    pool: Arc<LanguageServerPool>,
    sink: &RequestErrorSink,
) -> Vec<Diagnostic> {
    let result = dispatch_preferred(
        region_ctx,
        pool,
        send_diagnostic_fan_out_request,
        |v: &Vec<Diagnostic>| !v.is_empty(),
        None, // cancel handled at outer level
    )
    .await;
    count_no_winner_errors(&result, sink);

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
}
