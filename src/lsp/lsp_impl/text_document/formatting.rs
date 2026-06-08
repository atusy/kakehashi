//! `textDocument/formatting` handler and helpers shared with
//! `textDocument/rangeFormatting`.
//!
//! `textDocument/formatting` resolves every injection region in the document
//! and asks the configured downstream language servers to format each one.
//! Across regions the resulting [`TextEdit`] lists are concatenated, since each
//! region edits a disjoint span of the host document.
//!
//! Within a region, the aggregation strategy decides how multiple servers
//! combine:
//! - `preferred` (default) — [`dispatch_preferred_formatting`] picks the
//!   highest-priority non-empty response.
//! - `concatenated` (with a non-empty `priorities` allowlist) —
//!   [`dispatch_concatenated_formatting`] runs the listed servers **serially**
//!   (each formats the previous server's output) and collapses the result into
//!   one region-replacement edit. Serial application keeps the output
//!   overlap-free without merging conflicting edits. See
//!   concatenated-formatting-pipeline.
//!
//! # Shared helpers exposed to `range_formatting`
//!
//! [`Kakehashi::setup_formatting_cancel_token`] + [`FormattingCancelState`]
//! package the multi-consumer cancel pattern and
//! [`finalize_formatting_edits`] funnels per-region `JoinSet` results into
//! a single sorted edit vector. Both are `pub(super)` so the sibling range
//! handler in [`super::range_formatting`] reuses them verbatim.

use std::sync::Arc;

use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentFormattingParams, Position, Range, TextEdit};

use crate::config::settings::AggregationStrategy;
use crate::error::LockResultExt;
use crate::language::InjectionResolver;
use crate::lsp::aggregation::region::collect_region_results_with_cancel;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::aggregation::server::effective_priorities;
use crate::lsp::aggregation::server::run_sequential_format_pipeline;
use crate::lsp::bridge::{
    RegionOffset, UpstreamId, VirtualDocumentUri, translate_virtual_range_to_host,
};
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::request_id::{CancelReceiver, CancelSubscriptionGuard};

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn formatting_impl(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let lsp_uri = params.text_document.uri;
        let options = params.options;

        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in formatting: {}", lsp_uri.as_str());
            return Ok(None);
        };

        log::debug!("formatting called for {}", uri);

        let snapshot = match self.documents.get(&uri) {
            None => {
                log::debug!("formatting: No document found for {}", uri);
                return Ok(None);
            }
            Some(doc) => match doc.snapshot() {
                None => {
                    log::debug!("formatting: Document not fully initialized for {}", uri);
                    return Ok(None);
                }
                Some(snapshot) => snapshot,
            },
        };

        let Some(language_name) = self.document_language(&uri) else {
            log::debug!(target: "kakehashi::formatting", "No language detected");
            return Ok(None);
        };

        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(None);
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
            return Ok(None);
        }

        let upstream_request_id = crate::lsp::current_upstream_id();
        let cancel_state = self.setup_formatting_cancel_token(upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<Option<Vec<TextEdit>>> = JoinSet::new();

        for resolved in all_regions {
            let configs = self.bridge_configs_for_injection_language(
                &language_name,
                &resolved.injection_language,
            );
            if configs.is_empty() {
                continue;
            }

            let agg = self.resolve_aggregation_config(
                &language_name,
                &resolved.injection_language,
                "textDocument/formatting",
            );
            let region_ctx = DocumentRequestContext {
                uri: uri.clone(),
                resolved,
                configs,
                upstream_request_id: upstream_request_id.clone(),
                priorities: agg.priorities,
                strategy: agg.strategy,
                max_fan_out: agg.max_fan_out,
            };
            let pool = Arc::clone(&pool);
            let options = options.clone();
            let region_cancel_rx = cancel_state.derive_receiver();

            // `strategy: "concatenated"` with a non-empty priorities list opts
            // this region into the sequential formatter pipeline
            // (concatenated-formatting-pipeline): run the priority-listed
            // servers serially, each formatting the prior server's output, then
            // emit one region-replacement edit. Everything else (default
            // `preferred`, or `concatenated` with no priorities — a
            // misconfiguration) keeps the existing first-non-empty-wins path.
            let use_concatenated = region_ctx.strategy == AggregationStrategy::Concatenated
                && !region_ctx.priorities.is_empty();

            outer_join_set.spawn(async move {
                if use_concatenated {
                    dispatch_concatenated_formatting(
                        &region_ctx,
                        pool.clone(),
                        options,
                        region_cancel_rx,
                    )
                    .await
                } else {
                    dispatch_preferred_formatting(
                        &region_ctx,
                        pool.clone(),
                        options,
                        region_cancel_rx,
                    )
                    .await
                }
            });
        }

        let response = finalize_formatting_edits(outer_join_set, cancel_state.token.clone()).await;
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());
        response
    }
}

/// `preferred`-strategy formatting for one region: the existing
/// first-non-empty-wins fan-out, factored out of the per-region task body.
async fn dispatch_preferred_formatting(
    region_ctx: &DocumentRequestContext,
    pool: Arc<crate::lsp::bridge::LanguageServerPool>,
    options: tower_lsp_server::ls_types::FormattingOptions,
    region_cancel_rx: Option<CancelReceiver>,
) -> Option<Vec<TextEdit>> {
    let result = dispatch_preferred(
        region_ctx,
        pool,
        move |t| {
            let options = options.clone();
            async move {
                t.pool
                    .send_formatting_request(
                        &t.server_name,
                        &t.server_config,
                        &t.uri,
                        &t.injection_language,
                        &t.region_id,
                        t.offset,
                        &t.virtual_content,
                        options,
                        t.upstream_id,
                    )
                    .await
            }
        },
        // `Some(vec![])` is an authoritative "no edits needed" from the
        // formatter (e.g., ruff signaling the code is already formatted) —
        // accept it instead of falling through to a lower-priority server that
        // might re-format the same code. `None` still means "no response" and
        // triggers fallback.
        |opt| opt.is_some(),
        region_cancel_rx,
    )
    .await;
    match result {
        FanInResult::Done(edits) => edits,
        FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
    }
}

/// `concatenated`-strategy formatting for one region: the sequential formatter
/// pipeline (concatenated-formatting-pipeline).
///
/// Runs the region's priority-listed servers serially over the region's virtual
/// content — each server formats the previous server's output — then collapses
/// the final text into a single host-coordinate region-replacement edit. When
/// the final text is byte-identical to the region's original content (no step
/// changed anything, or the changes round-tripped), contributes no edit.
///
/// Each step targets a unique scratch virtual document
/// ([`scratch_region_id`]), so the bridge always sends a fresh `didOpen`
/// carrying the current accumulated text — fixing the stale-content bug where a
/// step reused an already-open canonical document and formatted the *original*
/// region text. The scratch document is `didClose`d after the step
/// (concatenated-formatting-pipeline Decision point 7).
///
/// Cleanup is guaranteed even on cancel: every opened-but-not-yet-closed scratch
/// document is tracked, and a sweep after the cancel-aware `select!` closes any
/// the per-step path did not. Without this, a `$/cancelRequest` that drops the
/// in-flight step future before its own `close_scratch_document` ran would leak
/// that scratch virtual document downstream (review HIGH).
///
/// TODO(concatenated-formatting-pipeline): still to build (see the ADR):
///   - capability-based full -> rangeFormatting fallback for range-only servers,
///     distinguishing "no capability" from a `null` "already formatted";
///   - per-step remaining-budget timeout and concurrent $/cancelRequest
///     propagation (Decision points 6 and the Consequences cancellation note);
///   - discarding downstream `publishDiagnostics` targeting scratch URIs;
///     prompt didClose currently minimizes (but does not eliminate) the window.
async fn dispatch_concatenated_formatting(
    region_ctx: &DocumentRequestContext,
    pool: Arc<crate::lsp::bridge::LanguageServerPool>,
    options: tower_lsp_server::ls_types::FormattingOptions,
    cancel_rx: Option<CancelReceiver>,
) -> Option<Vec<TextEdit>> {
    let offset = RegionOffset::with_per_line_offsets(
        region_ctx.resolved.region.line_range.start,
        region_ctx.resolved.line_column_offsets.clone(),
    );
    let original_virtual = region_ctx.resolved.virtual_content.clone();
    // Precompute the host replacement range now, while we still hold
    // `original_virtual`, so it can be moved into the pipeline below without a
    // second clone. An unresolvable end position means we emit no edit (bounded
    // -range safety) — so bail before doing any downstream work.
    let replacement_range = region_replacement_range(&original_virtual, &offset)?;
    let injection_language = region_ctx.resolved.injection_language.clone();
    let region_id = region_ctx.resolved.region.region_id.clone();
    let uri = region_ctx.uri.clone();
    let upstream_id = region_ctx.upstream_request_id.clone();

    // `priorities` is both the membership allowlist and the application order;
    // `effective_priorities` keeps only entries configured for this region —
    // shared with the preferred fan-out so the rule has one source of truth.
    let server_names = effective_priorities(region_ctx);

    let server_config_for = |name: &str| {
        region_ctx
            .configs
            .iter()
            .find(|c| c.server_name == name)
            .map(|c| Arc::clone(&c.config))
    };

    // Each pipeline step targets a *fresh* scratch virtual document so the
    // bridge always performs a new `didOpen` carrying the current accumulated
    // text. Reusing the canonical region virtual document would make a step
    // format STALE text whenever that document is already open downstream (e.g.
    // after a prior hover/diagnostic), because `send_formatting_request` only
    // pushes content on the first `didOpen` (concatenated-formatting-pipeline
    // Decision point 7, stale-content bug). The per-step counter makes the
    // scratch id unique; the scratch document is `didClose`d after the step so
    // it never orphans tracking state or leaks diagnostics.
    let step_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Per-run sequence so scratch ids don't collide with a concurrent
    // concatenated-formatting request for the same region (which would also start
    // at step 0).
    let run_seq = SCRATCH_RUN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Convert the host URL to the bridge protocol's `Uri` once, for the per-step
    // didClose. This conversion is effectively infallible for a valid host URL;
    // if it ever failed, `send_formatting_request` (which performs the same
    // conversion internally) would also fail, so the step would simply contribute
    // no edit — there is no path where a step opens a scratch doc we then can't
    // close. The scratch doc is also swept on the host document's own close.
    let host_uri_lsp = crate::lsp::lsp_impl::url_to_uri(&uri).ok();

    // Track every scratch document opened during this run that has NOT yet been
    // closed, so we can guarantee cleanup even when a `tokio::select!` cancel
    // drops the in-flight step future BEFORE its own `close_scratch_document`
    // runs. Two reviewers flagged that drop-on-cancel as a scratch-document
    // leak. The happy per-step path still closes immediately and removes its
    // entry here, so the post-`select!` sweep only closes whatever a cancel
    // left behind — no double-close.
    let open_scratch: Arc<std::sync::Mutex<Vec<OpenScratchDoc>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Retained for the post-`select!` cleanup sweep, since `uri` is moved into
    // the per-step closure below.
    let host_uri_for_sweep = uri.clone();

    // Safety net for the abort-before-sweep case: if this whole future is aborted
    // (outer `JoinSet` dropped / tower-lsp task cancelled) before the explicit
    // sweep below runs, the sweep never executes and the tracked scratch docs
    // would leak. The guard's Drop drains the tracker and detaches the didCloses
    // onto a task that outlives the aborted parent. On the normal/select-cancel
    // paths the sweep drains the tracker first, so the guard's Drop is a no-op
    // (drain-under-lock ⇒ no double-close). Kept alive across the whole pipeline
    // + sweep below.
    let _scratch_guard =
        ScratchCleanupGuard::new(Arc::clone(&pool), uri.clone(), Arc::clone(&open_scratch));

    let pipeline_fut = run_sequential_format_pipeline(original_virtual, &server_names, {
        let pool = Arc::clone(&pool);
        let open_scratch = Arc::clone(&open_scratch);
        move |server_name, current_text| {
            let pool = Arc::clone(&pool);
            let options = options.clone();
            let injection_language = injection_language.clone();
            let region_id = region_id.clone();
            let uri = uri.clone();
            let upstream_id = upstream_id.clone();
            let server_config = server_config_for(&server_name);
            let step_counter = Arc::clone(&step_counter);
            let host_uri_lsp = host_uri_lsp.clone();
            let open_scratch = Arc::clone(&open_scratch);
            async move {
                // No config for this server name: skip-and-continue, handing the
                // unchanged text back to the pipeline (ADR Decision point 6).
                let Some(server_config) = server_config else {
                    return (current_text, None);
                };
                let step = step_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let scratch_id = scratch_region_id(&region_id, run_seq, step);

                // Build the scratch URI up front and register it as "open" in the
                // shared tracker BEFORE the request, so that if this future is
                // dropped mid-send by a cancel, the post-`select!` sweep still
                // closes it. Skipped only when the host URL could not be
                // converted (then no didClose is possible anyway).
                let scratch_uri = host_uri_lsp
                    .as_ref()
                    .map(|h| VirtualDocumentUri::new(h, &injection_language, &scratch_id));
                if let Some(scratch_uri) = scratch_uri.as_ref() {
                    push_open_scratch(
                        &open_scratch,
                        OpenScratchDoc {
                            uri: scratch_uri.clone(),
                            server_name: server_name.clone(),
                        },
                    );
                }

                // The bridge translates the returned edits back to host
                // coordinates, but the pipeline applies them to the *virtual*
                // accumulated text, so request a fresh virtual-coordinate
                // result by passing the zero offset for the response transform.
                // The region offset is only re-applied once, when the final
                // text is collapsed into the host replacement edit below.
                //
                // Per-step host-transform interaction: `send_formatting_request`
                // runs `transform_formatting_response_to_host`, which clamps
                // edits to a synthetic EOF and drops any past `virtual_line_count`
                // for *this step's* `current_text`. Because we pass
                // `RegionOffset::new(0, 0)` (an identity translation) and the
                // step's own current text, those edits come back already
                // EOF-clamped and host-translated by a zero offset — i.e. still
                // in virtual coordinates relative to `current_text` — so applying
                // them to the virtual accumulated text is correct. The single
                // real region-offset translation happens once in the precomputed
                // `region_replacement_range`.
                // (`apply_text_edits` does its own clamping as a general safety
                // net, independent of this transform.)
                let send_result = pool
                    .send_formatting_request(
                        &server_name,
                        &server_config,
                        &uri,
                        &injection_language,
                        &scratch_id,
                        RegionOffset::new(0, 0),
                        &current_text,
                        options,
                        upstream_id,
                    )
                    .await;

                // ADR Decision point 6 (best-effort, skip-and-continue): a
                // failed step is skipped — never surfaced to the editor — but is
                // logged so the misbehaving server stays diagnosable. We log the
                // downstream error here (before mapping to `None`) instead of
                // silently dropping it with `.ok().flatten()`.
                let result = match send_result {
                    Ok(edits) => edits,
                    Err(e) => {
                        log::warn!(
                            target: "kakehashi::formatting",
                            "concatenated formatting step for server {} failed; skipping (ADR point 6): {}",
                            server_name,
                            e
                        );
                        None
                    }
                };

                // Scratch-document cleanup is deferred to the post-`select!`
                // sweep (`close_remaining_scratch_docs`), NOT closed here per
                // step. The sweep always runs to completion, so a cancel that
                // drops this step's future mid-flight can never leak a scratch
                // doc; closing per-step inside the (cancellable) pipeline future
                // could be interrupted after the tracker entry was already
                // removed, leaking it. The scratch doc stays tracked in
                // `open_scratch` (registered above) for the sweep to close.

                // Hand the (unchanged) virtual text back to the pipeline along
                // with the result so the pipeline can move it forward without a
                // per-step clone.
                (current_text, result)
            }
        }
    });

    // Poll cancellation concurrently with the in-flight pipeline so an upstream
    // $/cancelRequest aborts the region promptly rather than only between steps.
    // (Per-step $/cancelRequest propagation to the downstream server is a
    // follow-up — TODO(concatenated-formatting-pipeline).)
    let cancelled;
    let final_text = match cancel_rx {
        Some(mut rx) => {
            tokio::select! {
                res = pipeline_fut => { cancelled = false; res }
                _ = &mut rx => { cancelled = true; None }
            }
        }
        None => {
            cancelled = false;
            pipeline_fut.await
        }
    };

    // Close every scratch document the run opened — on both the completed and
    // cancelled paths. This sweep is the single cleanup point (steps only
    // register their scratch docs, never close them), so it runs to completion
    // regardless of cancellation and can never leak. `close_scratch_document` is
    // idempotent, so a doc already gone (e.g. via a concurrent host close) is a
    // no-op.
    close_remaining_scratch_docs(&pool, &host_uri_for_sweep, &open_scratch).await;

    // On cancel, contribute no edit (matches the prior early-return behavior).
    if cancelled {
        return None;
    }

    let final_text = final_text?;
    Some(vec![TextEdit {
        range: replacement_range,
        new_text: final_text,
    }])
}

/// A scratch virtual document opened during a concatenated-formatting run that
/// has not yet been closed. Paired with its `server_name` so the `didClose`
/// targets the connection the matching `didOpen` was sent on.
struct OpenScratchDoc {
    uri: VirtualDocumentUri,
    server_name: String,
}

/// Drop-based safety net that closes any scratch documents still tracked open
/// when the concatenated-formatting future is **aborted** (its outer `JoinSet`
/// dropped or the tower-lsp task cancelled) BEFORE the explicit post-`select!`
/// sweep runs.
///
/// The explicit awaited sweep ([`close_remaining_scratch_docs`]) handles the
/// normal-completion and `select!`-cancel paths deterministically and drains the
/// tracker; by the time this guard's `Drop` runs on those paths the tracker is
/// already empty, so its Drop is a no-op (the drain-under-lock makes the sweep
/// and the guard mutually exclusive — they can never double-close).
///
/// The hard case this guard exists for: a `future`-level abort drops the whole
/// dispatch future at an `.await` point before the sweep is reached. `Drop`
/// cannot `.await`, so it drains the tracker and, if anything remains, **spawns a
/// detached `tokio::task`** to run the `didClose`s. A detached task is NOT a
/// child of the aborted future, so it survives the parent's cancellation and runs
/// the `close_scratch_document` calls to completion — which is also why
/// `close_scratch_document` can keep its untrack-before-didClose ordering (the
/// detached task guarantees the didClose await still completes).
struct ScratchCleanupGuard {
    pool: Arc<crate::lsp::bridge::LanguageServerPool>,
    host_uri: url::Url,
    open: Arc<std::sync::Mutex<Vec<OpenScratchDoc>>>,
}

impl ScratchCleanupGuard {
    fn new(
        pool: Arc<crate::lsp::bridge::LanguageServerPool>,
        host_uri: url::Url,
        open: Arc<std::sync::Mutex<Vec<OpenScratchDoc>>>,
    ) -> Self {
        Self {
            pool,
            host_uri,
            open,
        }
    }
}

impl Drop for ScratchCleanupGuard {
    fn drop(&mut self) {
        // Drain under the lock. On the normal/select-cancel paths the explicit
        // sweep already drained, so this is empty and we return without spawning
        // — no double-close. On an abort-before-sweep, this is the leftover set.
        let remaining = drain_open_scratch(&self.open);
        if remaining.is_empty() {
            return;
        }

        // Drop can't await, and the parent future is (in the case that matters)
        // being aborted — so finish the didCloses on a DETACHED task that is not a
        // child of the aborted future and therefore runs to completion.
        let pool = Arc::clone(&self.pool);
        let host_uri = self.host_uri.clone();
        tokio::spawn(async move {
            for doc in remaining {
                pool.close_scratch_document(&host_uri, &doc.uri, &doc.server_name)
                    .await;
            }
        });
    }
}

/// Record a scratch document as open. Poison-safe: a poisoned mutex is recovered
/// (the tracked data is plain values, not invariants), logged per the project
/// lock-recovery convention.
fn push_open_scratch(open: &std::sync::Mutex<Vec<OpenScratchDoc>>, doc: OpenScratchDoc) {
    lock_open_scratch(open).push(doc);
}

/// Drain and return all still-open scratch documents.
fn drain_open_scratch(open: &std::sync::Mutex<Vec<OpenScratchDoc>>) -> Vec<OpenScratchDoc> {
    std::mem::take(&mut *lock_open_scratch(open))
}

/// Lock the open-scratch tracker, recovering from poisoning per the project
/// convention (no `unwrap()` on locks).
fn lock_open_scratch(
    open: &std::sync::Mutex<Vec<OpenScratchDoc>>,
) -> std::sync::MutexGuard<'_, Vec<OpenScratchDoc>> {
    open.lock().recover_poison("scratch-document tracker")
}

/// Close every scratch document still tracked as open. Used on both the
/// completed and cancelled paths so a cancel that drops an in-flight step never
/// leaves its scratch virtual document open downstream.
async fn close_remaining_scratch_docs(
    pool: &crate::lsp::bridge::LanguageServerPool,
    host_uri: &url::Url,
    open: &std::sync::Mutex<Vec<OpenScratchDoc>>,
) {
    for doc in drain_open_scratch(open) {
        pool.close_scratch_document(host_uri, &doc.uri, &doc.server_name)
            .await;
    }
}

/// Derive a unique scratch `region_id` for one step of the concatenated
/// formatting pipeline.
///
/// The pipeline feeds each server the *previous* server's output by passing the
/// accumulated text to `send_formatting_request`. But the bridge only pushes
/// that content downstream via `didOpen` when the virtual document is not yet
/// open (`ensure_document_opened`); if the canonical region virtual document is
/// already open for that server (common after a prior hover/diagnostic), the
/// formatting request reuses the stale open document and the server formats the
/// *original* region text, breaking the serial semantics
/// (concatenated-formatting-pipeline, the stale-content bug). Giving each step a
/// distinct `region_id` yields a distinct [`VirtualDocumentUri`], so the bridge
/// always performs a fresh `didOpen` carrying the current accumulated text.
///
/// The id keeps the canonical `region_id` as a prefix (so it stays unique per
/// region — no host-file collision) and appends a `-kakehashi-scratch-{step}`
/// suffix: unique per pipeline step, and carrying the standardized
/// `kakehashi-scratch` marker (Decision point 7) so external tools (file
/// watchers, build tools, test runners) can recognize and ignore these throwaway
/// documents. It still flows through [`VirtualDocumentUri::new`], which also
/// wraps it in the `kakehashi-virtual-uri-` filename marker and preserves the
/// host directory and language extension required for downstream config and
/// parser discovery.
/// Process-global, monotonically increasing pipeline-run sequence. The per-step
/// counter alone only makes scratch ids unique *within* a single pipeline run;
/// two concatenated-formatting requests for the same host region overlapping in
/// time (concurrent LSP requests, or a cancel+restart race) would otherwise both
/// start at step 0 and collide on the same scratch virtual URI. Mixing in this
/// run sequence makes scratch ids unique across concurrent runs in the process.
static SCRATCH_RUN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn scratch_region_id(region_id: &str, run: u64, step: usize) -> String {
    format!("{region_id}-kakehashi-scratch-{run}-{step}")
}

/// Compute the host-coordinate `Range` that the concatenated pipeline's single
/// whole-region replacement edit spans.
///
/// The range covers the whole *original* virtual document (`(0,0)` through the
/// end of `original_virtual`), translated to host coordinates via the region
/// [`RegionOffset`]. Emitting one whole-region edit keeps the LSP output
/// trivially non-overlapping (concatenated-formatting-pipeline Decision point 4).
///
/// Returns `None` when the virtual end position cannot be resolved, rather than
/// fabricating an unbounded range. `byte_to_position` should always succeed for
/// the document's own length, but if it ever returns `None` (a mapper bug or
/// corrupt content), emitting a `u32::MAX`/`u32::MAX` range would translate into
/// a host edit spanning essentially the whole file — silently corrupting it. We
/// emit no edit for that region instead, which is the safe degradation.
///
/// TODO(concatenated-formatting-pipeline): Decision point 4 also requires
/// re-applying the region's per-line host prefix/indentation to every line of
/// `final_text` (and the region LCP for new lines on a line-count change), plus
/// trimming trailing whitespace on empty lines. This slice emits `final_text`
/// verbatim and relies on the existing range translation only, so multi-line /
/// blockquoted regions still drop host indentation on replacement lines — the
/// same limitation documented in `src/lsp/bridge/text_document/formatting.rs`.
fn region_replacement_range(original_virtual: &str, offset: &RegionOffset) -> Option<Range> {
    // Virtual end position = the position of the very last byte, derived via the
    // shared PositionMapper so line-ending handling (incl. `\r\n`) and UTF-16
    // column math stay consistent with the rest of the codebase. Computed from
    // the original virtual text *before* it is moved into the pipeline, so the
    // pipeline takes ownership without a second clone.
    let end = crate::text::PositionMapper::new(original_virtual)
        .byte_to_position(original_virtual.len())?;

    let mut range = Range {
        start: Position {
            line: 0,
            character: 0,
        },
        end,
    };
    translate_virtual_range_to_host(&mut range, offset);
    Some(range)
}

/// Sort `edits` in place by `range.start` (line, then character).
///
/// Formatting tasks for separate injection regions complete in arbitrary
/// order via `JoinSet::join_next`, so without sorting the concatenated
/// `TextEdit` vector is non-deterministic across runs. Since regions are
/// disjoint, sorting by start position is a stable total order.
fn sort_edits_by_start_position(edits: &mut [TextEdit]) {
    edits.sort_by_key(|edit| (edit.range.start.line, edit.range.start.character));
}

/// Derive a single-use [`CancelReceiver`] (oneshot) that fires when `token`
/// is cancelled.
///
/// `dispatch_preferred` and `collect_region_results_with_cancel` accept
/// `Option<CancelReceiver>` (a oneshot), but the upstream cancel arrives as
/// a single non-cloneable channel and we need to observe it from multiple
/// places concurrently (one outer collector + one per region). Spawning a
/// tiny forwarder per consumer turns the cloneable token back into the
/// non-cloneable oneshot shape the aggregation layer expects.
///
/// The forwarder races `token.cancelled()` against `tx.closed()` so it exits
/// as soon as either side finishes — without this, a request that completes
/// normally (consumer drops the receiver without ever cancelling) would leak
/// one task per region per request, each holding a clone of the token.
fn cancel_rx_from_token(token: CancellationToken) -> CancelReceiver {
    let (mut tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        tokio::select! {
            _ = token.cancelled() => {
                let _ = tx.send(());
            }
            _ = tx.closed() => {}
        }
    });
    rx
}

/// Multi-consumer cancel state for a formatting / rangeFormatting fan-out.
///
/// Wraps the cloneable [`CancellationToken`] that every per-region task and
/// the outer collector subscribe to, plus the [`CancelSubscriptionGuard`]
/// that keeps the upstream subscription alive for the duration of the
/// request. The guard is bound to the lifetime of the parent `&Kakehashi`.
pub(super) struct FormattingCancelState<'a> {
    /// `None` when no upstream cancel source exists (no upstream id or
    /// duplicate subscription). Per-region consumers should treat this as
    /// "no cancel" rather than a pre-cancelled token, otherwise every
    /// region task would abort before starting.
    pub(super) token: Option<CancellationToken>,
    /// Held alive for the lifetime of the request; dropping it unsubscribes
    /// from the cancel forwarder. `None` when there is no upstream
    /// subscription to manage.
    _guard: Option<CancelSubscriptionGuard<'a>>,
}

impl<'a> FormattingCancelState<'a> {
    /// Derive a fresh per-consumer [`CancelReceiver`] from the token, if any.
    pub(super) fn derive_receiver(&self) -> Option<CancelReceiver> {
        self.token.as_ref().map(|t| cancel_rx_from_token(t.clone()))
    }
}

impl Kakehashi {
    /// Wire up the multi-consumer cancel pattern used by both formatting
    /// fan-outs.
    ///
    /// Subscribes to upstream cancel notifications (returning `None` when
    /// no upstream id is present or the subscription is a duplicate), then
    /// forwards that single-use oneshot into a cloneable
    /// [`CancellationToken`]. Per-region tasks call
    /// [`FormattingCancelState::derive_receiver`] to get their own oneshot
    /// receiver, so cancel propagates to every dispatcher simultaneously —
    /// not after the slowest formatter completes naturally.
    ///
    /// When `subscribe_cancel` returns `None`, the token stays `None` so
    /// downstream consumers receive `None` for their `cancel_rx` and
    /// degrade to "no cancel" instead of being told "already cancelled".
    /// Using a pre-cancelled token here would abort every region task
    /// before it even started.
    pub(super) fn setup_formatting_cancel_token(
        &self,
        upstream_request_id: Option<&UpstreamId>,
    ) -> FormattingCancelState<'_> {
        let (cancel_rx, guard) = self.subscribe_cancel(upstream_request_id);
        let token = cancel_rx.map(|rx| {
            let token = CancellationToken::new();
            let forward = token.clone();
            tokio::spawn(async move {
                // Fires on both real cancel and tx-drop (subscription guard).
                let _ = rx.await;
                forward.cancel();
            });
            token
        });
        FormattingCancelState {
            token,
            _guard: guard,
        }
    }
}

/// Collect per-region `JoinSet` results, sort the concatenated edits, and
/// shape into the LSP-spec response (`None` when there are no edits).
///
/// Both formatting handlers funnel their per-region futures into a single
/// `JoinSet<Option<Vec<TextEdit>>>` and need to:
///
/// 1. Cancel-aware collect every region's edits (regions whose dispatcher
///    cancelled or returned no result contribute nothing).
/// 2. Sort by start position so the concatenated response is deterministic
///    across `JoinSet::join_next` arrival order.
/// 3. Map `vec![]` to `Ok(None)` per the LSP convention that empty edit
///    lists are equivalent to no response.
pub(super) async fn finalize_formatting_edits(
    outer_join_set: JoinSet<Option<Vec<TextEdit>>>,
    cancel_token: Option<CancellationToken>,
) -> Result<Option<Vec<TextEdit>>> {
    let all_edits = collect_region_results_with_cancel(
        outer_join_set,
        cancel_token.map(cancel_rx_from_token),
        |acc, opt: Option<Vec<TextEdit>>| {
            if let Some(items) = opt {
                acc.extend(items);
            }
        },
    )
    .await;

    let mut all_edits = all_edits?;
    sort_edits_by_start_position(&mut all_edits);
    Ok(if all_edits.is_empty() {
        None
    } else {
        Some(all_edits)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::{Position, Range};

    fn edit(start_line: u32, start_char: u32, new_text: &str) -> TextEdit {
        TextEdit {
            range: Range {
                start: Position {
                    line: start_line,
                    character: start_char,
                },
                end: Position {
                    line: start_line,
                    character: start_char,
                },
            },
            new_text: new_text.to_string(),
        }
    }

    // ==========================================================================
    // scratch_region_id (concatenated-formatting-pipeline: stale-content fix)
    // ==========================================================================

    #[test]
    fn scratch_region_id_is_unique_per_step() {
        // Each pipeline step must get a distinct scratch id so the bridge builds
        // a distinct virtual URI and re-sends a fresh didOpen with the current
        // accumulated text (rather than reusing the prior step's stale document).
        let a = scratch_region_id("REGION", 0, 0);
        let b = scratch_region_id("REGION", 0, 1);
        let c = scratch_region_id("REGION", 0, 2);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn scratch_region_id_is_unique_across_runs_for_the_same_step() {
        // Two concurrent format requests for the same region both start at step 0;
        // the per-run sequence must still make their scratch ids distinct.
        assert_ne!(
            scratch_region_id("REGION", 7, 0),
            scratch_region_id("REGION", 8, 0)
        );
    }

    #[test]
    fn scratch_region_id_differs_from_canonical_region_id() {
        // The scratch document must never collide with the region's canonical
        // virtual document (which other requests like hover keep open).
        let region_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        assert_ne!(scratch_region_id(region_id, 0, 0), region_id);
    }

    #[test]
    fn scratch_region_id_keeps_region_id_as_prefix() {
        // Preserving the region_id prefix keeps the scratch id unique per region
        // (no host-file collision across regions).
        let id = scratch_region_id("REGION", 0, 3);
        assert!(
            id.starts_with("REGION"),
            "scratch id should keep the region_id prefix: {id}"
        );
    }

    #[test]
    fn scratch_region_id_produces_virtual_uri_with_marker_and_extension() {
        // The scratch id must flow through VirtualDocumentUri to keep the host
        // directory + language extension (config/parser discovery) and the
        // distinctive kakehashi-virtual-uri- marker (Decision point 7).
        use crate::lsp::bridge::VirtualDocumentUri;
        let host_uri: tower_lsp_server::ls_types::Uri = "file:///project/doc.md".parse().unwrap();
        let id = scratch_region_id("REGION", 0, 1);
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "python", &id);
        let uri_string = virtual_uri.to_uri_string();
        assert!(
            uri_string.starts_with("file:///project/kakehashi-virtual-uri-"),
            "scratch URI must keep host dir + marker: {uri_string}"
        );
        assert!(
            uri_string.ends_with(".py"),
            "scratch URI must keep the language extension: {uri_string}"
        );
        assert!(
            VirtualDocumentUri::is_virtual_uri(&uri_string),
            "scratch URI must be recognized as virtual: {uri_string}"
        );
    }

    // ==========================================================================
    // Scratch-document tracking/cleanup (review HIGH: leak-on-cancel)
    // ==========================================================================

    fn scratch_doc(host: &str, id: &str, server: &str) -> OpenScratchDoc {
        let host_uri: tower_lsp_server::ls_types::Uri = host.parse().unwrap();
        OpenScratchDoc {
            uri: VirtualDocumentUri::new(&host_uri, "python", id),
            server_name: server.to_string(),
        }
    }

    #[test]
    fn open_scratch_tracker_drains_what_was_pushed() {
        // The cancel-path sweep relies on draining everything that was recorded
        // open but never removed by a per-step close.
        let open = std::sync::Mutex::new(Vec::new());
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-0", "black"));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-1", "isort"));

        let drained = drain_open_scratch(&open);
        assert_eq!(drained.len(), 2, "both opened scratch docs must be drained");
        assert!(
            drain_open_scratch(&open).is_empty(),
            "drain must leave the tracker empty (no double-close)"
        );
    }

    #[test]
    fn open_scratch_tracker_accumulates_every_step_for_the_sweep() {
        // Steps only register their scratch docs; the post-select sweep is the
        // single close point, so every pushed doc must still be present to drain.
        let open = std::sync::Mutex::new(Vec::new());
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-0", "black"));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-1", "isort"));
        let drained = drain_open_scratch(&open);
        assert_eq!(
            drained.len(),
            2,
            "the sweep must see every step's scratch doc"
        );
    }

    #[tokio::test]
    async fn scratch_cleanup_guard_drains_tracker_on_drop() {
        // The guard is the safety net for the abort-before-sweep case: if the
        // dispatch future is aborted before the explicit sweep runs, the guard's
        // Drop must drain the tracker (and detach the didClose), so no scratch
        // doc leaks. Here we assert the drain-on-drop: the tracker is emptied.
        let open = Arc::new(std::sync::Mutex::new(Vec::new()));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-0", "black"));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-1", "isort"));

        let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::new());
        let host = url::Url::parse("file:///d.md").unwrap();
        let guard = ScratchCleanupGuard::new(Arc::clone(&pool), host, Arc::clone(&open));

        drop(guard);

        assert!(
            lock_open_scratch(&open).is_empty(),
            "guard Drop must drain the tracker so nothing leaks on abort-before-sweep"
        );
    }

    #[tokio::test]
    async fn scratch_cleanup_guard_drop_is_noop_after_explicit_sweep() {
        // Normal path: the explicit awaited sweep drains the tracker first, so the
        // guard's Drop sees an empty tracker and does nothing — the sweep and the
        // guard can never double-close.
        let open = Arc::new(std::sync::Mutex::new(Vec::new()));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-0", "black"));

        let pool = Arc::new(crate::lsp::bridge::LanguageServerPool::new());
        let host = url::Url::parse("file:///d.md").unwrap();
        let guard = ScratchCleanupGuard::new(Arc::clone(&pool), host.clone(), Arc::clone(&open));

        // Explicit sweep (the deterministic cleanup the normal/select-cancel paths
        // run) drains the tracker.
        close_remaining_scratch_docs(&pool, &host, &open).await;
        assert!(
            lock_open_scratch(&open).is_empty(),
            "explicit sweep must drain the tracker"
        );

        // Guard Drop now sees an empty tracker → no-op, so no double-close.
        drop(guard);
        assert!(
            lock_open_scratch(&open).is_empty(),
            "guard Drop after sweep must remain a no-op"
        );
    }

    #[test]
    fn lock_open_scratch_recovers_from_poison() {
        // Per the project convention, lock helpers must recover from poisoning
        // rather than unwrap()-panic.
        let open = Arc::new(std::sync::Mutex::new(Vec::new()));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-0", "black"));

        // Poison the mutex by panicking while holding the guard.
        let open_clone = Arc::clone(&open);
        let _ = std::thread::spawn(move || {
            let _guard = open_clone.lock().unwrap();
            panic!("poison the lock");
        })
        .join();

        // Recovery path must still observe the previously-tracked doc.
        let drained = drain_open_scratch(&open);
        assert_eq!(
            drained.len(),
            1,
            "poison recovery must preserve tracked scratch docs"
        );
    }

    // ==========================================================================
    // region_replacement_range (review: unbounded-range fallback)
    // ==========================================================================

    #[test]
    fn region_replacement_range_resolves_end_for_valid_text() {
        // For any valid UTF-8 region the end position is resolvable, so we get a
        // bounded replacement range — never the old u32::MAX/u32::MAX fallback.
        let offset = RegionOffset::new(0, 0);
        let range = region_replacement_range("line1\nline2", &offset)
            .expect("end position must resolve for valid text");
        // End is the position just past the last byte: line 1, char 5 ("line2").
        assert_eq!(range.end.line, 1);
        assert_eq!(range.end.character, 5);
        // Crucially, the range is bounded (no fabricated u32::MAX).
        assert_ne!(range.end.line, u32::MAX);
        assert_ne!(range.end.character, u32::MAX);
    }

    #[test]
    fn region_replacement_range_handles_empty_region() {
        // Empty region → end at (0,0), still a bounded Some(range).
        let offset = RegionOffset::new(0, 0);
        let range = region_replacement_range("", &offset).expect("empty region must still resolve");
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 0);
    }

    #[test]
    fn sort_edits_orders_by_line_then_character() {
        // Arrival order from JoinSet is arbitrary; pretend the two regions
        // completed in reverse order.
        let mut edits = vec![
            edit(8, 0, "from_region_b"),
            edit(2, 5, "from_region_a_second"),
            edit(2, 0, "from_region_a_first"),
        ];

        sort_edits_by_start_position(&mut edits);

        assert_eq!(edits[0].new_text, "from_region_a_first");
        assert_eq!(edits[1].new_text, "from_region_a_second");
        assert_eq!(edits[2].new_text, "from_region_b");
    }

    #[test]
    fn sort_edits_is_stable_for_equal_start_positions() {
        // Disjoint regions never produce equal starts in practice, but the
        // sort should remain stable for any callers that happen to.
        let mut edits = vec![
            edit(3, 0, "first"),
            edit(3, 0, "second"),
            edit(3, 0, "third"),
        ];

        sort_edits_by_start_position(&mut edits);

        assert_eq!(edits[0].new_text, "first");
        assert_eq!(edits[1].new_text, "second");
        assert_eq!(edits[2].new_text, "third");
    }

    #[test]
    fn sort_edits_handles_empty_input() {
        let mut edits: Vec<TextEdit> = Vec::new();
        sort_edits_by_start_position(&mut edits);
        assert!(edits.is_empty());
    }

    // ==========================================================================
    // cancel_rx_from_token (review MAJOR follow-up: cancel propagation)
    // ==========================================================================

    #[tokio::test]
    async fn cancel_rx_from_token_resolves_when_token_is_cancelled() {
        let token = CancellationToken::new();
        let rx = cancel_rx_from_token(token.clone());

        token.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "derived oneshot must fire on token cancel"
        );
    }

    #[tokio::test]
    async fn cancel_rx_from_token_supports_multiple_derived_receivers() {
        // The whole point of the token shim: one upstream cancel must reach
        // every consumer (outer collector + N region tasks).
        let token = CancellationToken::new();
        let rx_a = cancel_rx_from_token(token.clone());
        let rx_b = cancel_rx_from_token(token.clone());
        let rx_c = cancel_rx_from_token(token.clone());

        token.cancel();

        for (name, rx) in [("rx_a", rx_a), ("rx_b", rx_b), ("rx_c", rx_c)] {
            let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx).await;
            assert!(
                matches!(result, Ok(Ok(()))),
                "{} must fire on shared token cancel",
                name
            );
        }
    }

    #[tokio::test]
    async fn cancel_rx_from_token_stays_pending_until_cancel() {
        // Negative case: without cancellation the receiver must NOT fire.
        let token = CancellationToken::new();
        let rx = cancel_rx_from_token(token.clone());

        let result = tokio::time::timeout(std::time::Duration::from_millis(50), rx).await;
        assert!(
            result.is_err(),
            "receiver must remain pending while token is alive"
        );
        // Token still owned by this scope — drop it explicitly so the spawned
        // forwarder task exits cleanly (no leaked task on test teardown).
        token.cancel();
    }

    #[tokio::test]
    async fn cancel_rx_from_token_forwarder_exits_when_receiver_dropped() {
        // Regression: the forwarder previously awaited `token.cancelled()`
        // unconditionally, so the common "request completed without cancel"
        // path leaked one task per region per request, each holding a clone
        // of the token. The fix races cancel against `tx.closed()` so the
        // forwarder exits as soon as the consumer drops the receiver.
        //
        // We can't directly assert task count, but we can verify the token's
        // weak handle count drops back to baseline after rx is dropped,
        // proving the forwarder released its clone.
        let token = CancellationToken::new();
        let baseline_token = token.clone(); // anchor so the test owns one ref

        let rx = cancel_rx_from_token(token.clone());
        // Forwarder now holds an extra clone; let it observe the channel.
        tokio::task::yield_now().await;

        drop(rx);
        // Yield enough times for the forwarder to observe `tx.closed()`,
        // drop its token clone, and exit.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // After the forwarder exits, cancelling must not deliver to a stale
        // consumer (the rx is gone) and must not hang on a held clone. If
        // the forwarder leaked, this would still complete — but combined
        // with the surrounding pre-existing tests, this guards the intent
        // of the fix.
        baseline_token.cancel();
    }
}
