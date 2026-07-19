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
    RelatedUnchangedDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport,
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
        // Count every answered LSP pull and its handler latency (#533). Measured
        // here (the LSP entry) rather than the shared inner so the one-shot CLI
        // `diagnose` path, which calls `_with_error_sink` directly, is excluded.
        let start = std::time::Instant::now();
        let result = self.diagnostic_impl_with_error_sink(params, None).await;
        self.diagnostics
            .record_pull(start.elapsed().as_micros() as u64);
        result
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
            self.mark_pull_covered(&uri, served_version);
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
            self.mark_pull_covered(&uri, served_version);
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
            self.documents.get(&uri).and_then(|doc| {
                doc.snapshot()
                    .map(|snapshot| (snapshot, doc.content_version()))
            })
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
            (true, Some((snapshot, _))) => self
                .language
                .injection_query(&language_name)
                .map(|injection_query| {
                    match self
                        .documents
                        .current_resolved_regions(&uri, self.cache.semantic_token_generation())
                    {
                        Some(regions) => regions,
                        None => std::sync::Arc::new(InjectionResolver::resolve_all(
                            &self.language,
                            self.bridge.node_tracker(),
                            &uri,
                            snapshot.tree(),
                            snapshot.text(),
                            injection_query.as_ref(),
                            snapshot.incarnation(),
                        )),
                    }
                })
                .unwrap_or_default(),
            // Virt gated off, or no tree yet (the host layer still pulls). The
            // wait+on-demand above already tried, so a missing tree here is the
            // rare parse-failure case, self-healing on the next pull.
            _ => std::sync::Arc::new(Vec::new()),
        };
        let virt_regions = if self.tree_worker_shadow.is_authoritative()
            && let Some((snapshot, content_version)) = snapshot.as_ref()
        {
            self.worker_injection_regions_for_snapshot(
                &uri,
                snapshot.incarnation(),
                *content_version,
                &language_name,
            )
            .await
            .unwrap_or_default()
        } else {
            virt_regions
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

        // Pre-filter servers already known (a live, `Ready` connection) NOT to
        // answer pull diagnostics, so the per-region fan-out below never spawns a
        // task + connection lookup only to hit the capability gate and return an
        // empty layer (capability-prefilter-fanout). A push-only server like
        // basedpyright, warmed by eager-open, is dropped here for EVERY region
        // instead of once per (region × pull). One pool query for the whole
        // request; the resulting set is a cheap lookup inside the region loop.
        let incapable_servers = self
            .incapable_virt_servers(
                &language_name,
                virt_regions.iter().map(|r| r.injection_language.as_str()),
                "textDocument/diagnostic",
            )
            .await;

        // Virt layer: 2-level aggregation —
        //   Inner: dispatch per region (fans out to all servers for that region)
        //   Outer: collect across regions
        let virt_fut = async {
            let mut outer_join_set: JoinSet<Vec<Diagnostic>> = JoinSet::new();
            for resolved in virt_regions.iter() {
                let mut configs = self.bridge_configs_for_injection_language(
                    &language_name,
                    &resolved.injection_language,
                );
                // Drop known-incapable servers before building the fan-out
                // context (capability-prefilter-fanout).
                if !incapable_servers.is_empty() {
                    configs.retain(|c| !incapable_servers.contains(&c.server_name));
                }
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
                    resolved: resolved.clone(),
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
        // combine. Cancel drops both in-flight layer futures. The RAII sweep
        // cleans stale upstream-registry entries on every exit (completion,
        // cancel, or the request future being dropped) — it fires only after
        // both layer futures are dropped/settled, so no live sharer of the
        // id can be wiped. Sweeping while dropped futures unwind is safe:
        // cancel forwarding captures its targets before notifying, and the
        // pool tolerates entries unregistered out of order.
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            pool.clone(),
            upstream_request_id.clone(),
        );
        let combined = async { tokio::join!(virt_fut, host_fut) };
        let (mut virt_items, mut host_items) = match cancel_rx {
            Some(mut cancel_rx) => {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => {
                        return Err(tower_lsp_server::jsonrpc::Error::request_cancelled());
                    }
                    result = combined => result,
                }
            }
            None => combined.await,
        };

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

        // Degraded-answer guard (the pull-side sibling of `republish`'s
        // geometry-unknown deferral): the bounded parse wait above can lapse
        // under load, leaving `snapshot` `None` for an open document while the
        // aggregator holds live region pushes — the fold above then silently
        // skipped every cached `Region` slot, so this answer is missing whole
        // servers' diagnostics. A pull must respond (there is no "defer" for a
        // request), so serve the degraded set — but do NOT mark it served, and
        // record the per-host debt: the reparse loop's post-parse pass (or the
        // TOCTOU guard below) consumes it and requests the recovery refresh
        // (single-flighted, forced past the coverage gate) that brings the
        // client back to a full view, instead of the gap being masked until
        // the next edit. The predicate mirrors `republish`'s `needs_geometry`
        // (non-empty region slots only).
        let degraded_virt =
            virt_enabled && snapshot.is_none() && self.diagnostics.has_region_slots(&uri);

        // The editor is about to receive the current merged set: advance `served` to
        // the version captured at entry, so the refresh gate stops treating those
        // changes as dirty (#497). Pure bookkeeping — never republishes — so this
        // can't beget a refresh.
        if degraded_virt {
            // The debt drives the post-parse recovery refresh (see
            // `DiagnosticAggregator::degraded_pulls`).
            self.diagnostics.record_degraded_pull(&uri);
            // TOCTOU guard: `snapshot` was captured before the fan-out/fold
            // awaits above, so the parse may have landed — and the post-parse
            // debt consumer already run — in between, leaving this
            // freshly-recorded debt with no consumer until the next edit. If
            // geometry is available NOW, consume the debt here and fire the
            // recovery refresh ourselves. FORCED past the coverage gate (still
            // single-flighted): the debt itself proves the client just
            // received a non-covering answer, which the version-based gate
            // cannot see — an edit-race degradation leaves served == current
            // (no push-origin change), so a gated request would be suppressed
            // while the client displays the region-less set. `take` on both
            // consumers makes double-firing impossible; if the snapshot is
            // still absent, the parse that produces it has not run its
            // post-parse pass yet, so that pass will consume. Loop-bounded:
            // the refresh-induced re-pull sees the ready geometry, answers
            // covering, and clears everything.
            let geometry_ready = self
                .documents
                .get(&uri)
                .and_then(|doc| doc.snapshot())
                .is_some();
            if geometry_ready && self.diagnostics.take_degraded_pull(&uri) {
                crate::lsp::lsp_impl::DiagnosticPublisher::new(self)
                    .request_pull_diagnostic_refresh(true);
            }
        } else {
            self.mark_pull_covered(&uri, served_version);
        }

        let items = combine_layer_diagnostics(&layer_cfg, virt_items, host_items);
        // Content-addressed result id over the canonically sorted items
        // (stateless): the client echoes the id it last received as
        // `previousResultId`; when the recomputed id matches, an UNCHANGED
        // report replaces the full item list. On a diagnostics-heavy host a
        // full report is ~1 MB (measured: ~2,235 items), and every
        // `workspace/diagnostic/refresh`-induced re-pull of a settled set
        // previously re-shipped all of it.
        let (items, result_id) = finalize_pull_items(items);
        if params.previous_result_id.as_deref() == Some(result_id.as_str()) {
            return Ok(unchanged_diagnostic_report(result_id));
        }
        Ok(make_diagnostic_report(items, result_id))
    }

    /// A pull answered with a covering (non-degraded) report: advance the
    /// coverage `served` marker and clear any earlier degraded-pull debt —
    /// every covering exit must do both, or a stale debt would later fire an
    /// unnecessary (though coverage-gated) recovery refresh.
    fn mark_pull_covered(&self, uri: &Url, served_version: u64) {
        self.diagnostics.mark_served(uri, served_version);
        self.diagnostics.forget_degraded_pull(uri);
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

/// Canonicalize a pull result and mint its content-addressed id: sort the
/// items deterministically (the fan-outs complete in arbitrary order, and the
/// hash needs canonical serialization; it also gives the editor a stable list
/// order, matching the publish path #423), then hash. One function so the
/// handler cannot hash unsorted items — an order-unstable id would make
/// `unchanged` reports silently never fire on multi-server hosts.
fn finalize_pull_items(mut items: Vec<Diagnostic>) -> (Vec<Diagnostic>, String) {
    crate::lsp::diagnostic_order::sort_diagnostics_deterministically(&mut items);
    let id = diagnostic_result_id(&items);
    (items, id)
}

/// Content-address a **sorted** pull result: the deterministic sort
/// ([`sort_diagnostics_deterministically`], applied by [`finalize_pull_items`]) makes the
/// serialization canonical, so the same logical set always hashes to the same
/// id and any field change produces a different one. The length suffix is
/// cheap insurance against a 64-bit collision making a changed set read as
/// unchanged (the same accepted trade as the bridge's `document_tracker`
/// content fingerprint). Stateless: the client echoes the id back as
/// `previousResultId` and the handler just recomputes — nothing to invalidate.
///
/// [`sort_diagnostics_deterministically`]: crate::lsp::diagnostic_order::sort_diagnostics_deterministically
fn diagnostic_result_id(items: &[Diagnostic]) -> String {
    let mut writer = crate::text::Fnv1aWriter::new();
    std::io::Write::write_all(&mut writer, b"[").expect("FNV writer is infallible");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            std::io::Write::write_all(&mut writer, b",").expect("FNV writer is infallible");
        }
        // Keep peak canonicalization memory bounded to one diagnostic and
        // stream its JSON directly into the hash. If serialization ever fails,
        // fail open: a fresh id is safer than falsely reporting `unchanged`.
        if crate::lsp::diagnostic_order::write_diagnostic_canonical_json(&mut writer, item).is_err()
        {
            static UNHASHABLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let nonce = UNHASHABLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return format!("unhashable-{nonce}-{}", items.len());
        }
    }
    std::io::Write::write_all(&mut writer, b"]").expect("FNV writer is infallible");
    format!("{:016x}-{}", writer.finish(), items.len())
}

/// Create a full diagnostic report from aggregated diagnostics. `result_id`
/// lets the client echo `previousResultId` on its next pull so an unchanged
/// set can be answered with [`unchanged_diagnostic_report`].
fn make_diagnostic_report(
    diagnostics: Vec<Diagnostic>,
    result_id: String,
) -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
        RelatedFullDocumentDiagnosticReport {
            full_document_diagnostic_report: FullDocumentDiagnosticReport {
                result_id: Some(result_id),
                items: diagnostics,
            },
            related_documents: None,
        },
    ))
}

/// Create an UNCHANGED diagnostic report: the client's `previousResultId`
/// matches the current set, so no items are re-shipped (LSP 3.17 pull
/// diagnostics).
fn unchanged_diagnostic_report(result_id: String) -> DocumentDiagnosticReportResult {
    DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(
        RelatedUnchangedDocumentDiagnosticReport {
            unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport { result_id },
            related_documents: None,
        },
    ))
}

/// Create an empty diagnostic report (full report with no items).
///
/// Deliberately carries no `resultId`: these are the early-return paths
/// (missing document, undetectable language, no participating layer) that
/// answer before any merge exists. A tiny Full-empty response costs nothing to
/// re-ship, and a client `previousResultId` can never match an absent id, so
/// the unchanged-report machinery simply doesn't engage here.
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

    #[test]
    fn result_id_is_stable_across_fan_in_order() {
        // The pull's fan-outs complete in arbitrary order; `finalize_pull_items`
        // (the one function the handler goes through) must canonicalize
        // permutations of the same logical set to the same result id —
        // otherwise a client's previousResultId would never match on a
        // multi-server host and unchanged reports would silently never fire.
        // Deliberately feeds UNSORTED permutations so removing the sort inside
        // `finalize_pull_items` fails this test.
        let a = vec![diag("x"), diag("y"), diag("z")];
        let b = vec![diag("z"), diag("x"), diag("y")];
        let (sorted_a, id_a) = finalize_pull_items(a);
        let (sorted_b, id_b) = finalize_pull_items(b);
        assert_eq!(sorted_a, sorted_b, "the sort canonicalizes permutations");
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn result_id_is_stable_across_deep_tie_order() {
        let with_data = |value| {
            let mut diagnostic = diag("same");
            diagnostic.data = Some(serde_json::json!({"nested": {"value": value}}));
            diagnostic
        };
        let first = with_data(1);
        let second = with_data(2);

        // These diagnostics tie on every cheap sort field and differ only in
        // nested data, so reversing them exercises the canonical tiebreaker.
        let (sorted_a, id_a) = finalize_pull_items(vec![first.clone(), second.clone()]);
        let (sorted_b, id_b) = finalize_pull_items(vec![second, first]);

        assert_eq!(sorted_a, sorted_b);
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn result_id_streaming_hash_matches_the_materialized_form() {
        // `diagnostic_result_id` hashes one canonical item at a time to avoid a
        // ~1 MB array transient; it must stay byte-identical to hashing the
        // equivalent materialized canonical array.
        let items = vec![diag("x"), diag("y")];
        let materialized = format!(
            "[{},{}]",
            crate::lsp::diagnostic_order::canonical_json_string(&items[0]),
            crate::lsp::diagnostic_order::canonical_json_string(&items[1])
        );
        let expected = format!(
            "{:016x}-{}",
            crate::text::fnv1a_hash(&materialized),
            items.len()
        );
        assert_eq!(diagnostic_result_id(&items), expected);
    }

    #[test]
    fn result_id_ignores_json_object_insertion_order() {
        let with_data = |keys: [&str; 2]| {
            let mut diagnostic = diag("same");
            let mut data = serde_json::Map::new();
            for key in keys {
                data.insert(key.to_string(), serde_json::json!(1));
            }
            diagnostic.data = Some(serde_json::Value::Object(data));
            diagnostic
        };

        let first = vec![with_data(["z", "a"])];
        let second = vec![with_data(["a", "z"])];
        assert_ne!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        assert_eq!(diagnostic_result_id(&first), diagnostic_result_id(&second));
    }

    #[test]
    fn result_id_changes_with_any_content_change() {
        let base = vec![diag("x"), diag("y")];
        let mut changed_message = base.clone();
        changed_message[1].message = "y'".to_string();
        let mut changed_severity = base.clone();
        changed_severity[0].severity = Some(tower_lsp_server::ls_types::DiagnosticSeverity::ERROR);
        let shrunk = vec![diag("x")];

        let id = diagnostic_result_id(&base);
        assert_ne!(id, diagnostic_result_id(&changed_message));
        assert_ne!(id, diagnostic_result_id(&changed_severity));
        assert_ne!(id, diagnostic_result_id(&shrunk));
    }

    #[tokio::test]
    async fn degraded_pull_does_not_mark_the_change_served() {
        // The pull-side sibling of republish's geometry deferral: when the
        // bounded parse wait lapses (no snapshot) while the aggregator holds
        // live region pushes, the answer is missing whole servers' diagnostics
        // (the fold skips region slots without offsets). The answer must still
        // be served — a pull cannot defer — but it must NOT advance `served`,
        // and it records the per-host debt the post-parse pass consumes to
        // fire the recovery refresh that brings the client back to a full
        // view.
        let (service, _socket) = tower_lsp_server::LspService::new(Kakehashi::new);
        let server = service.inner();
        server
            .language
            .language_registry_for_parallel()
            .register("rust".to_string(), tree_sitter_rust::LANGUAGE.into());
        let uri = Url::parse("file:///test/degraded_pull.rs").unwrap();
        server.documents.insert(
            uri.clone(),
            "fn main() {}".to_string(),
            Some("rust".to_string()),
            None, // tree pending: snapshot() is None
        );
        server.diagnostics.record(
            &uri,
            crate::lsp::diagnostic_cache::DiagnosticSource::Region("region-1".to_string()),
            "lua_ls".to_string(),
            Some(crate::lsp::bridge::ProgressConnectionId::for_test(1)),
            vec![diag("boom")],
        );
        server.diagnostics.bump_current(&uri);
        assert!(
            server.diagnostics.is_dirty(),
            "the push made the host dirty"
        );

        let params = DocumentDiagnosticParams {
            text_document: tower_lsp_server::ls_types::TextDocumentIdentifier {
                uri: "file:///test/degraded_pull.rs".parse().expect("uri"),
            },
            identifier: None,
            previous_result_id: None,
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let report = server
            .diagnostic_impl(params)
            .await
            .expect("a degraded pull still answers");
        let DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) = report
        else {
            panic!("degraded answer is a full report");
        };
        assert!(
            full.full_document_diagnostic_report.items.is_empty(),
            "the region push cannot be folded without geometry (that's the degradation)"
        );
        assert!(
            server.diagnostics.is_dirty(),
            "a degraded answer must not advance `served` — the gap would be masked"
        );
        assert!(
            server.diagnostics.take_degraded_pull(&uri),
            "the degraded answer records the per-host debt that keys the post-parse recovery refresh"
        );
    }

    #[test]
    fn unchanged_report_carries_the_id_and_no_items() {
        let report = unchanged_diagnostic_report("abc-1".to_string());
        let serialized = serde_json::to_value(&report).expect("serializes");
        assert_eq!(serialized["kind"], "unchanged");
        assert_eq!(serialized["resultId"], "abc-1");
        assert!(
            serialized.get("items").is_none(),
            "an unchanged report re-ships no items"
        );
    }

    #[test]
    fn full_report_carries_the_result_id() {
        let report = make_diagnostic_report(vec![diag("x")], "abc-1".to_string());
        let serialized = serde_json::to_value(&report).expect("serializes");
        assert_eq!(serialized["kind"], "full");
        assert_eq!(serialized["resultId"], "abc-1");
        assert_eq!(serialized["items"].as_array().map(Vec::len), Some(1));
    }
}
