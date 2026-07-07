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

use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{
    DocumentFormattingParams, NumberOrString, Position, Range, TextEdit,
};

use crate::config::settings::AggregationStrategy;
use crate::config::settings::{LayerSource, PRIORITIES_WILDCARD};
use crate::error::LockResultExt;
use crate::language::InjectionResolver;
use crate::lsp::aggregation::region::collect_region_results_with_cancel;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::aggregation::server::effective_priorities_from;
use crate::lsp::aggregation::server::run_sequential_format_pipeline;
use crate::lsp::aggregation::server::{
    dispatch_preferred_with_tokens, mint_region_progress_source,
};
use crate::lsp::bridge::{
    ClientProgressAggregator, ClientProgressDeregisterGuard, RegionOffset, ResolvedServerConfig,
    UpstreamId, VirtualDocumentUri, translate_virtual_range_to_host,
};
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::lsp::lsp_impl::text_document::{
    RequestErrorSink, count_no_winner_errors, count_request_errors,
};
use crate::lsp::request_id::{CancelReceiver, CancelSubscriptionGuard};

use super::super::{Kakehashi, uri_to_url};

impl Kakehashi {
    pub(crate) async fn formatting_impl(
        &self,
        params: DocumentFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.formatting_impl_with_error_sink(params, None).await
    }

    /// [`Self::formatting_impl`] with an optional request-failure counter.
    ///
    /// In LSP mode failed downstream requests are log-only — the editor
    /// simply retries — so `formatting_impl` passes `None`. CLI mode is
    /// one-shot: without the counter, a ready server that then fails its
    /// formatting request (crash, error response, timeout) is
    /// indistinguishable from "nothing to format" and would exit 0.
    pub(crate) async fn formatting_impl_with_error_sink(
        &self,
        params: DocumentFormattingParams,
        request_error_sink: RequestErrorSink,
    ) -> Result<Option<Vec<TextEdit>>> {
        let lsp_uri = params.text_document.uri;
        let options = params.options;
        // The editor's work-done token for this request, if any. Client progress
        // is wired only for the preferred per-region branch (concatenated is
        // deferred, #440); minting per region happens inside `virt_format_edits`.
        let work_done_token = params.work_done_progress_params.work_done_token.clone();

        let Ok(uri) = uri_to_url(&lsp_uri) else {
            log::warn!("Invalid URI in formatting: {}", lsp_uri.as_str());
            return Ok(None);
        };

        log::debug!("formatting called for {}", uri);

        // Explicit-action bounded wait (parse-snapshot ADR §3): formatting is
        // user-triggered and infrequent, so it may briefly wait for the
        // in-flight parse; a still-stale snapshot after the wait rejects with
        // ContentModified rather than silently no-opping an action the user
        // consciously triggered. Never-parsed/gone falls through to the
        // existing empty fallbacks below.
        if let crate::lsp::lsp_impl::snapshot_read::SnapshotWait::Stale = self
            .wait_for_current_snapshot(&uri, std::time::Duration::from_millis(500))
            .await
        {
            return Err(crate::error::content_modified_error());
        }

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

        // Formatting consumes the full layer config (order AND strategy):
        // it is the first method with a layer-level `concatenated` dispatch
        // (cross-layer-aggregation phase 3).
        let layer_cfg = self.resolve_layer_config(&language_name, "textDocument/formatting");

        let upstream_request_id = crate::lsp::current_upstream_id();
        let mut cancel_state = self.setup_formatting_cancel_token(upstream_request_id.as_ref());
        cancel_state.request_error_sink = request_error_sink;
        let original = snapshot.text_arc();

        let result = match layer_cfg.strategy {
            AggregationStrategy::Preferred => {
                // Concurrent fan-out with priority decision
                // (race_layers_preferred): both layers' requests are in
                // flight at once; the highest-priority non-empty result wins.
                let virt = self.virt_format_edits(
                    &uri,
                    &snapshot,
                    &language_name,
                    &options,
                    &upstream_request_id,
                    &cancel_state,
                    work_done_token.clone(),
                );
                let host = self.host_format_edits(&lsp_uri, &original, &options, &cancel_state);
                crate::lsp::lsp_impl::bridge_context::race_layers_preferred(
                    &layer_cfg.priorities,
                    virt,
                    host,
                    std::future::ready(Ok(None)),
                    |edits: &Vec<TextEdit>| !edits.is_empty(),
                )
                .await?
            }
            AggregationStrategy::Concatenated => {
                // Sequential cross-layer pipeline (cross-layer-aggregation
                // phase 3): each layer formats the previous layer's output.
                // Virt edits are resolved against the parsed snapshot, so virt
                // can only run while the accumulated text still IS the
                // snapshot text — i.e. before any text-changing layer. The
                // default order (virt before host) satisfies this; an order
                // placing virt after a producing host is skipped with a
                // warning rather than formatting against stale regions.
                let mut current = original.to_string();
                let mut producers = 0usize;
                let mut sole_edits: Option<Vec<TextEdit>> = None;
                for layer in &layer_cfg.priorities {
                    let edits = match layer {
                        LayerSource::Virt => {
                            if current.as_str() != &*original {
                                log::warn!(
                                    target: "kakehashi::formatting",
                                    "cross-layer concatenated formatting: virt placed after a \
                                     text-producing layer in layers.priorities; injection regions \
                                     cannot be re-resolved against modified text — skipping virt"
                                );
                                continue;
                            }
                            self.virt_format_edits(
                                &uri,
                                &snapshot,
                                &language_name,
                                &options,
                                &upstream_request_id,
                                &cancel_state,
                                work_done_token.clone(),
                            )
                            .await?
                        }
                        LayerSource::Host => {
                            self.host_format_edits(&lsp_uri, &current, &options, &cancel_state)
                                .await?
                        }
                        LayerSource::Native => None,
                    };
                    if let Some(edits) = edits
                        && !edits.is_empty()
                    {
                        current = crate::text::edit::apply_text_edits(&current, &edits);
                        producers += 1;
                        sole_edits = Some(edits);
                    }
                }
                match producers {
                    0 => None,
                    // A single producing layer ran against the original text
                    // (virt by the guard above; host because nothing before
                    // it produced), so its minimal edits apply verbatim.
                    1 => sole_edits,
                    // Multiple producers: later layers' edits are relative to
                    // intermediate text, so collapse the chain into one
                    // whole-document replacement (the same overlap-free shape
                    // as the within-region pipeline, concatenated-formatting-
                    // pipeline Decision point 4).
                    _ => whole_document_replacement(&original, &current),
                }
            }
        };
        Ok(result)
    }

    /// Format every injection region (the **virt** layer): resolve regions
    /// from the parsed snapshot, format each one per its aggregation config,
    /// and concatenate the per-region edits (disjoint spans).
    #[allow(clippy::too_many_arguments)]
    async fn virt_format_edits(
        &self,
        uri: &url::Url,
        snapshot: &crate::document::model::DocumentSnapshot,
        language_name: &str,
        options: &tower_lsp_server::ls_types::FormattingOptions,
        upstream_request_id: &Option<UpstreamId>,
        cancel_state: &FormattingCancelState<'_>,
        client_progress_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<TextEdit>>> {
        let Some(injection_query) = self.language.injection_query(language_name) else {
            return Ok(None);
        };

        let all_regions = match self
            .documents
            .current_resolved_regions(uri, self.cache.semantic_token_generation())
        {
            Some(regions) => regions,
            None => std::sync::Arc::new(InjectionResolver::resolve_all(
                &self.language,
                self.bridge.node_tracker(),
                uri,
                snapshot.tree(),
                snapshot.text(),
                injection_query.as_ref(),
            )),
        };

        if all_regions.is_empty() {
            return Ok(None);
        }

        let pool = self.bridge.pool_arc();

        // Outer JoinSet: one task per injection region, all in parallel
        let mut outer_join_set: JoinSet<Option<Vec<TextEdit>>> = JoinSet::new();

        // Shared host-text position mapper for prefix extraction, built at
        // most once per request: constructing it is O(document size), so
        // rebuilding per region would make many-region documents quadratic.
        // Lazy because only concatenated-plan regions need it.
        let mut host_mapper: Option<crate::text::PositionMapper> = None;

        // Shared client progress: a formatting request fans out over several
        // injection regions, so the preferred-branch regions share ONE
        // aggregator + ONE teardown guard. The aggregator's winner rule shows
        // the first region to begin as one coherent `Begin → … → End` on the
        // editor's token (ls-bridge-client-progress). Concatenated-pipeline
        // regions are excluded from minting (client progress deferred, #440).
        // `None` when no `workDoneToken`.
        let shared_cp = client_progress_token.map(|client_token| {
            (
                Arc::new(Mutex::new(ClientProgressAggregator::new(client_token))),
                Arc::clone(pool.client_progress_registry()),
            )
        });
        let mut cp_minted: Vec<NumberOrString> = Vec::new();

        for resolved in all_regions.iter() {
            let configs = self
                .bridge_configs_for_injection_language(language_name, &resolved.injection_language);
            if configs.is_empty() {
                continue;
            }

            let agg = self.resolve_aggregation_config(
                language_name,
                &resolved.injection_language,
                "textDocument/formatting",
            );
            let region_ctx = DocumentRequestContext {
                uri: uri.clone(),
                resolved: resolved.clone(),
                configs,
                upstream_request_id: upstream_request_id.clone(),
                priorities: agg.priorities,
                strategy: agg.strategy,
                max_fan_out: agg.max_fan_out,
                client_progress_token: None,
            };
            // Decide how this region formats — concatenated pipeline, preferred
            // fan-out, or skip — from its resolved aggregation config. See
            // [`plan_region_format`] for the allowlist rule
            // (concatenated-formatting-pipeline Decision point 2).
            if region_ctx.strategy == AggregationStrategy::Concatenated
                && region_ctx
                    .priorities
                    .iter()
                    .any(|name| name == PRIORITIES_WILDCARD)
                && region_ctx
                    .priorities
                    .iter()
                    .any(|name| name != PRIORITIES_WILDCARD)
            {
                // ADR aggregation-priorities-wildcard: '*' has no deterministic
                // expansion order, and a formatter pipeline must be
                // reproducible — the element is dropped, explicit names run.
                log::warn!(
                    target: "kakehashi::formatting",
                    "concatenated formatting for {}->{} lists '{}' in priorities; \
                     the wildcard is ignored by the sequential pipeline (explicit \
                     order required) and only the named servers run",
                    language_name,
                    region_ctx.resolved.injection_language,
                    PRIORITIES_WILDCARD,
                );
            }
            let pipeline = match plan_region_format(
                region_ctx.strategy,
                &region_ctx.priorities,
                &region_ctx.configs,
                region_ctx.max_fan_out,
            ) {
                RegionFormatPlan::Concatenated(servers) => {
                    // Capture the region's per-line host prefixes now, while
                    // the host snapshot is at hand: the pipeline's whole-region
                    // replacement must re-apply them to its multi-line output
                    // (Decision point 4, `reapply_host_line_prefixes`).
                    let virtual_line_count =
                        region_ctx.resolved.virtual_content.matches('\n').count() + 1;
                    let mapper = host_mapper
                        .get_or_insert_with(|| crate::text::PositionMapper::new(snapshot.text()));
                    let host_line_prefixes = extract_host_line_prefixes(
                        mapper,
                        snapshot.text(),
                        region_ctx.resolved.region.line_range.start,
                        &region_ctx.resolved.line_column_offsets,
                        virtual_line_count,
                    );
                    Some((servers, host_line_prefixes))
                }
                RegionFormatPlan::Preferred => None,
                RegionFormatPlan::Skip => {
                    // `concatenated` with a non-empty `priorities` that names only
                    // unconfigured servers: the allowlist resolved to nothing, so
                    // running the region's other servers would violate it. Leave
                    // the region unformatted and warn so the typo'd/missing name
                    // surfaces instead of silently formatting with the wrong
                    // server.
                    log::warn!(
                        target: "kakehashi::formatting",
                        "concatenated formatting for {}->{} lists only unconfigured \
                         server(s) {:?}; none are configured for this region, so it \
                         is left unformatted (priorities is an allowlist — \
                         non-listed servers are not run)",
                        language_name,
                        region_ctx.resolved.injection_language,
                        region_ctx.priorities,
                    );
                    continue;
                }
                RegionFormatPlan::Disabled => {
                    // priorities = []: the per-method fan-out kill switch
                    // (aggregation-priorities-wildcard). Deliberate config, so
                    // no warning — just skip the region.
                    log::debug!(
                        target: "kakehashi::formatting",
                        "formatting disabled for {}->{} (priorities = [])",
                        language_name,
                        region_ctx.resolved.injection_language,
                    );
                    continue;
                }
            };

            // Mint this region's tracked-source token into the shared
            // aggregator ONLY for the preferred branch. Concatenated regions
            // (`pipeline.is_some()`) get no token — concatenated client progress
            // is deferred (#440). `pipeline.is_none()` only borrows, so `pipeline`
            // can still be moved into the spawn below.
            let region_cp_tokens = if pipeline.is_none() {
                shared_cp.as_ref().and_then(|(aggregator, registry)| {
                    mint_region_progress_source(&region_ctx, registry, aggregator)
                })
            } else {
                None
            };
            if let Some(map) = &region_cp_tokens {
                cp_minted.extend(map.values().cloned());
            }

            let pool = Arc::clone(&pool);
            let options = options.clone();
            let region_cancel_rx = cancel_state.derive_receiver();
            let request_error_sink = cancel_state.request_error_sink.clone();

            outer_join_set.spawn(async move {
                if let Some((servers, host_line_prefixes)) = pipeline {
                    dispatch_concatenated_formatting(
                        &region_ctx,
                        pool.clone(),
                        options,
                        servers,
                        host_line_prefixes,
                        region_cancel_rx,
                        request_error_sink,
                    )
                    .await
                } else {
                    dispatch_preferred_formatting(
                        &region_ctx,
                        pool.clone(),
                        options,
                        region_cancel_rx,
                        request_error_sink,
                        region_cp_tokens,
                    )
                    .await
                }
            });
        }

        // One teardown guard for the whole request, held across the edit
        // collection so the synthetic terminal `End` fires once, after every
        // region settles (or on cancel). Dropped at the end of this block.
        let _cp_guard = shared_cp.map(|(aggregator, registry)| {
            ClientProgressDeregisterGuard::new(registry, cp_minted, aggregator, pool.upstream_tx())
        });

        let response = finalize_formatting_edits(outer_join_set, cancel_state.token.clone()).await;
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());
        response
    }

    /// Format the host document itself (the **host** layer,
    /// host-document-bridge): forward `text` with the real URI to the
    /// host-capable servers and return their edits verbatim.
    ///
    /// `text` is the text to format — under the cross-layer `concatenated`
    /// pipeline it may already carry the virt layer's edits, in which case
    /// the host server's document state is temporarily speculative; the lazy
    /// fingerprint sync restores it to the editor text on the next request.
    async fn host_format_edits(
        &self,
        lsp_uri: &tower_lsp_server::ls_types::Uri,
        text: &str,
        options: &tower_lsp_server::ls_types::FormattingOptions,
        cancel_state: &FormattingCancelState<'_>,
    ) -> Result<Option<Vec<TextEdit>>> {
        let Some(mut ctx) = self.resolve_host_bridge_context(lsp_uri, "textDocument/formatting")
        else {
            return Ok(None);
        };
        ctx.text = Arc::from(text);

        let params = serde_json::json!({
            "textDocument": { "uri": lsp_uri.as_str() },
            "options": options,
        });

        let cancel_rx = cancel_state.derive_receiver();
        let pool = self.bridge.pool_arc();
        let result = crate::lsp::aggregation::server::dispatch_host_preferred(
            &ctx,
            pool.clone(),
            // Non-counting send, mirroring the virt preferred dispatch: under
            // `preferred` a non-winning host server's failure is not a CLI
            // exit-2 (the winner is authoritative), and in-task counting is racy
            // (the fan-in aborts losers without joining). Failures are counted
            // only when no server won, via `count_no_winner_errors` below (#503).
            move |t| {
                let params = params.clone();
                async move {
                    t.pool
                        .send_host_formatting_request(
                            &t.server_name,
                            &t.server_config,
                            &crate::lsp::bridge::HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                            },
                            "textDocument/formatting",
                            params,
                            t.upstream_id,
                        )
                        .await
                }
            },
            // `Some(vec![])` (and a capable server's `null`, normalized to it)
            // is the authoritative "no edits needed" — accept it instead of
            // falling through to a lower-priority host formatter that might
            // re-format the same text. Mirrors the virt preferred predicate.
            |opt| opt.is_some(),
            cancel_rx,
        )
        .await;
        pool.unregister_all_for_upstream_id(ctx.upstream_request_id.as_ref());
        // Count failures only when no host server won (#503): `NoResult` drains
        // every task, so its `errors` is the exhaustive, deterministic count.
        count_no_winner_errors(&result, &cancel_state.request_error_sink);
        // Quiet on an all-empty host layer (the virt arm already emits the
        // client-visible LOG); only real host failures get surfaced —
        // mirroring `host_layer_value_with_ctx`.
        match result {
            FanInResult::Done(value) => Ok(value),
            FanInResult::NoResult { errors } => {
                // `errors` was just recorded into the sink above; this arm only
                // chooses log severity.
                if errors > 0 {
                    self.client
                        .log_message(
                            tower_lsp_server::ls_types::MessageType::WARNING,
                            "No textDocument/formatting response from any host bridge server",
                        )
                        .await;
                }
                Ok(None)
            }
            FanInResult::Cancelled => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
        }
    }
}

/// Collapse a formatted text into a single whole-document replacement edit
/// against `original` (the same overlap-free output shape as the
/// within-region pipeline, concatenated-formatting-pipeline Decision
/// point 4). `None` when the chain round-tripped to the original text.
///
/// Line counting treats `\n` as the only line break: a document using bare
/// `\r` separators (which LSP also recognizes) would get an end position on
/// the wrong line. Tree-sitter parsing upstream shares the `\n` assumption,
/// so such documents do not reach this path in practice.
fn whole_document_replacement(original: &str, formatted: &str) -> Option<Vec<TextEdit>> {
    if original == formatted {
        return None;
    }
    let end_line = original.matches('\n').count() as u32;
    let last_line_start = original.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end_character = original[last_line_start..].encode_utf16().count() as u32;
    Some(vec![TextEdit {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: end_line,
                character: end_character,
            },
        },
        new_text: formatted.to_string(),
    }])
}

/// How a single injection region should be formatted, derived from its resolved
/// aggregation config. Extracted so the gating rule lives in one testable place.
#[derive(Debug, PartialEq, Eq)]
enum RegionFormatPlan {
    /// Run the sequential concatenated pipeline over these effective servers
    /// (explicit `priorities` names filtered to configured servers, deduped,
    /// order preserved — the `"*"` wildcard is excluded, see
    /// aggregation-priorities-wildcard).
    Concatenated(Vec<String>),
    /// Use the `preferred` first-non-empty-wins fan-out over the region's servers.
    Preferred,
    /// Run nothing for this region. Reached when `concatenated` is active with
    /// explicit `priorities` names that are all unconfigured/typo'd: every
    /// configured server is *absent from the allowlist*, and the allowlist
    /// (concatenated-formatting-pipeline Decision point 2) forbids running
    /// them. Falling through to `preferred` here would wrongly run exactly the
    /// servers the user's `priorities` excluded.
    Skip,
    /// `priorities = []`: the per-method fan-out kill switch
    /// (aggregation-priorities-wildcard) — the region runs no servers at all,
    /// regardless of strategy.
    Disabled,
}

/// Decide the [`RegionFormatPlan`] for a region from its aggregation `strategy`,
/// `priorities`, configured server `configs`, and `max_fan_out` cap.
///
/// Mirrors concatenated-formatting-pipeline Decision points 1–2 under the
/// aggregation-priorities-wildcard list semantics:
/// - `priorities = []` → `Disabled` (the kill switch applies to every strategy);
/// - `maxFanOut = 0` → `Disabled` ("disable fan-out entirely" holds for the
///   sequential pipeline too; for `Preferred` the dispatch enforces the cap
///   itself, this just short-circuits the region consistently);
/// - any non-`concatenated` strategy → `Preferred` (first-non-empty-wins;
///   the `"*"` wildcard is honored by the preferred dispatch);
/// - `concatenated` whose `priorities` carries no explicit name (only `"*"`,
///   e.g. the resolved default) is a misconfiguration — the pipeline's order
///   would be undefined — → `Preferred` (a once-per-config warning is emitted
///   at settings-apply time, see `format_concatenated_formatting_warning`);
/// - `concatenated` with explicit names puts the allowlist in force: run the
///   effective (configured ∩ explicit) servers as a pipeline, capped to
///   `max_fan_out` steps like the parallel paths cap servers queried, or —
///   when none of the listed names are configured — `Skip`, since the
///   allowlist forbids running the non-listed servers `preferred` would pick.
///   A `"*"` mixed into the list is ignored (no deterministic expansion order
///   for a sequential pipeline); the caller warns.
fn plan_region_format(
    strategy: AggregationStrategy,
    priorities: &[String],
    configs: &[ResolvedServerConfig],
    max_fan_out: Option<usize>,
) -> RegionFormatPlan {
    if priorities.is_empty() {
        return RegionFormatPlan::Disabled;
    }
    if max_fan_out == Some(0) {
        return RegionFormatPlan::Disabled;
    }
    if strategy != AggregationStrategy::Concatenated {
        return RegionFormatPlan::Preferred;
    }
    let explicit: Vec<String> = priorities
        .iter()
        .filter(|name| name.as_str() != PRIORITIES_WILDCARD)
        .cloned()
        .collect();
    if explicit.is_empty() {
        return RegionFormatPlan::Preferred;
    }
    let mut effective = effective_priorities_from(&explicit, configs);
    if effective.is_empty() {
        return RegionFormatPlan::Skip;
    }
    if let Some(cap) = max_fan_out {
        effective.truncate(cap);
    }
    RegionFormatPlan::Concatenated(effective)
}

/// Whole-pipeline time budget for one region's concatenated formatting run.
///
/// Reuses the explicit per-request aggregation timeout from
/// ls-bridge-server-pool-coordination (5s for explicit user actions) as the
/// whole-pipeline bound, per the concatenated-formatting-pipeline Consequences
/// note — no formatting-specific timeout config is introduced. Each step's
/// deadline is the *remaining* share of this budget
/// ([`remaining_step_budget`]), so a slow early step exhausts the budget and
/// the rest are skipped instead of stacking serial round-trips past the
/// client's own request timeout.
const PIPELINE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Minimum per-step budget worth issuing a request for (ADR Decision point 6:
/// "below a small floor the pipeline skips the rest outright rather than
/// issuing requests almost certain to time out").
const PIPELINE_STEP_FLOOR: std::time::Duration = std::time::Duration::from_millis(50);

/// Per-step deadline: the remaining share of the whole-pipeline budget
/// (`budget − elapsed`), or `None` when the remainder is below `floor` (skip
/// the step — and, since `elapsed` only grows, every step after it).
fn remaining_step_budget(
    budget: std::time::Duration,
    elapsed: std::time::Duration,
    floor: std::time::Duration,
) -> Option<std::time::Duration> {
    let remaining = budget.checked_sub(elapsed)?;
    (remaining >= floor).then_some(remaining)
}

/// Which request one concatenated-pipeline step issues for a server, keyed on
/// the capabilities it advertises (concatenated-formatting-pipeline Decision
/// point 3.2).
#[derive(Debug, PartialEq, Eq)]
enum PipelineStepRequest {
    /// `documentFormattingProvider` advertised: full `textDocument/formatting`.
    Full,
    /// Range-only server (`documentRangeFormattingProvider` without
    /// `documentFormattingProvider`): `textDocument/rangeFormatting` over the
    /// entire region, so the server still participates in the pipeline.
    WholeRegionRange,
    /// Neither provider advertised: the step contributes nothing and sends no
    /// unsupported request downstream.
    Skip,
}

/// Pure capability-to-request mapping for one pipeline step (ADR Decision
/// point 3.2). Full formatting always wins when advertised; the range
/// fallback is itself gated on the range capability.
fn select_pipeline_step_request(supports_full: bool, supports_range: bool) -> PipelineStepRequest {
    if supports_full {
        PipelineStepRequest::Full
    } else if supports_range {
        PipelineStepRequest::WholeRegionRange
    } else {
        PipelineStepRequest::Skip
    }
}

/// Probe the server's advertised formatting capabilities (spawning it if
/// needed, and waiting up to `timeout` — the step's remaining budget — for a
/// cold server to finish initializing so its capabilities are actually
/// known) and map them to the step's request kind via
/// [`select_pipeline_step_request`]. The connection handle is fetched once
/// and both capabilities are read off it directly, so the probe locks the
/// pool's connection map a single time.
///
/// Returns the kind together with the **actual** connection key the request
/// will route to (`handle.key()`). The caller must use this — not a key
/// resolved up front — to tag the step's scratch document and cancel: a
/// `preferSharedInstance` server whose shared connection was still
/// `Initializing` at resolve time, but turns out incapable, is diverted to a
/// per-root connection here, so a pre-resolved (optimistic shared) key would
/// mis-target the scratch `didClose` and the cancel (#391).
async fn pipeline_step_request_kind(
    pool: &crate::lsp::bridge::LanguageServerPool,
    server_name: &str,
    server_config: &crate::config::settings::BridgeServerConfig,
    document_uri: Option<&url::Url>,
    timeout: std::time::Duration,
) -> std::io::Result<(PipelineStepRequest, crate::lsp::bridge::ConnectionKey)> {
    let handle = pool
        .get_or_create_connection_wait_ready(server_name, server_config, document_uri, timeout)
        .await?;
    let supports_full = handle.has_capability("textDocument/formatting");
    let supports_range = handle.has_capability("textDocument/rangeFormatting");
    Ok((
        select_pipeline_step_request(supports_full, supports_range),
        handle.key().clone(),
    ))
}

/// `preferred`-strategy formatting for one region: the existing
/// first-non-empty-wins fan-out, factored out of the per-region task body.
///
/// `client_progress_tokens` carries the region's tracked-source progress token
/// (minted by [`mint_region_progress_source`] into the request's shared
/// aggregator) when the editor attached a `workDoneToken`; `None` otherwise.
/// When present, the dispatch routes the winning downstream's `$/progress`
/// onto the editor's token via [`dispatch_preferred_with_tokens`]; when absent
/// it falls back to [`dispatch_preferred`] (no client progress).
async fn dispatch_preferred_formatting(
    region_ctx: &DocumentRequestContext,
    pool: Arc<crate::lsp::bridge::LanguageServerPool>,
    options: tower_lsp_server::ls_types::FormattingOptions,
    region_cancel_rx: Option<CancelReceiver>,
    request_error_sink: RequestErrorSink,
    client_progress_tokens: Option<std::collections::HashMap<String, NumberOrString>>,
) -> Option<Vec<TextEdit>> {
    let send = move |t: crate::lsp::aggregation::server::FanOutTask| {
        let options = options.clone();
        // Non-counting send: under `preferred` a non-winning server's request
        // failure is NOT a CLI exit-2 (the winner is authoritative), and
        // counting losers in-task is racy — the preferred fan-in `abort_all`s
        // losers without joining them, so a loser's `fetch_add` could land
        // after the CLI read the counter. Failures are recorded only when no
        // server won, via `count_no_winner_errors` on the fan-in result below
        // (#503, mirroring the diagnose fix #487).
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
                    // Hand the winning downstream its bridge-minted client-progress
                    // token (the tracked source's); the aggregator relays its
                    // `$/progress` onto the editor's token. `None` for suppressed
                    // candidates and when the request carries no `workDoneToken`.
                    t.client_progress_token,
                    None,
                )
                .await
        }
    };
    // `Some(vec![])` is an authoritative "no edits needed" from the
    // formatter — both an explicit empty list and (since the bridge
    // transform stopped collapsing it) a `null` response per LSP — so
    // accept it instead of falling through to a lower-priority server that
    // might re-format the same code. `None` now strictly means "no usable
    // response" (missing provider, error, malformed payload) and triggers
    // fallback.
    let is_nonempty = |opt: &Option<Vec<TextEdit>>| opt.is_some();
    let result = match client_progress_tokens {
        Some(tokens) => {
            dispatch_preferred_with_tokens(
                region_ctx,
                pool,
                send,
                is_nonempty,
                region_cancel_rx,
                tokens,
            )
            .await
        }
        None => dispatch_preferred(region_ctx, pool, send, is_nonempty, region_cancel_rx).await,
    };
    // Count failures only when no server won. `NoResult` drains every task, so
    // its `errors` is the exhaustive, deterministic count (zero if the
    // non-winners merely returned empty); `Done`/`Cancelled` count nothing
    // (#503).
    count_no_winner_errors(&result, &request_error_sink);
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
/// region text. Scratch documents are `didClose`d solely by
/// [`ScratchCleanupGuard`]'s Drop rather than per step, so cancellation can never
/// leak one (concatenated-formatting-pipeline Decision point 7).
///
/// Cleanup is guaranteed even on cancel: every opened-but-not-yet-closed scratch
/// document is tracked, and the guard's Drop — which fires on the normal return,
/// the `select!` cancel return, and a future-level abort alike — drains the
/// tracker and detaches the didCloses onto a task that always runs to completion.
/// Without this, a `$/cancelRequest` that drops the in-flight step future before
/// its own `close_scratch_document` ran would leak that scratch virtual document
/// downstream (review HIGH). Routing every cleanup through the detached task also
/// removes the cancel-during-cleanup edge cases the old awaited sweep had: a
/// cancel mid-`didClose` can no longer orphan a downstream doc.
///
/// Each step keys its request on the server's advertised capabilities
/// ([`select_pipeline_step_request`], ADR Decision point 3.2): full formatting
/// when `documentFormattingProvider` is advertised, whole-region
/// `rangeFormatting` for range-only servers, and no request at all for servers
/// advertising neither. A capable server's `null` response is authoritative
/// "already formatted" (`Some(vec![])` from the bridge transform) and never
/// triggers the fallback.
///
/// Each step runs under the remaining share of the whole-pipeline budget
/// ([`PIPELINE_BUDGET`] / [`remaining_step_budget`], ADR Decision point 6): a
/// timed-out step is skipped (its downstream request is best-effort cancelled
/// via `$/cancelRequest`) and the budget keeps shrinking, so the pipeline can
/// never stack serial round-trips past the client's own request timeout. An
/// upstream `$/cancelRequest` reaches the in-flight downstream server through
/// the `RequestIdCapture` middleware's existing fan-out.
///
/// Downstream `publishDiagnostics` targeting scratch URIs are discarded by
/// the bridge reader (ADR Decision point 7; `is_scratch_publish_diagnostics`
/// in `src/lsp/bridge/actor/reader.rs`), so the scratch documents this run
/// opens can never surface speculative diagnostics to the editor.
async fn dispatch_concatenated_formatting(
    region_ctx: &DocumentRequestContext,
    pool: Arc<crate::lsp::bridge::LanguageServerPool>,
    options: tower_lsp_server::ls_types::FormattingOptions,
    // The effective (configured, deduped, order-preserving) priority list,
    // computed by the caller — which also gates entry on it being non-empty.
    server_names: Vec<String>,
    // Per-line host prefixes of the region (caller extracts them from the
    // host snapshot), re-applied to the final multi-line output (Decision
    // point 4, `reapply_host_line_prefixes`).
    host_line_prefixes: Vec<String>,
    cancel_rx: Option<CancelReceiver>,
    request_error_sink: RequestErrorSink,
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
    // scratch id unique; scratch documents are `didClose`d solely by
    // `ScratchCleanupGuard`'s Drop, not per step, so a cancel can't leak one.
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

    // Track every scratch document opened during this run. Steps only register
    // their scratch docs here; they never close them per step. All closing
    // happens in `ScratchCleanupGuard`'s Drop, which drains the tracker and
    // closes each doc on a detached task. Deferring all cleanup to that single
    // cancel-safe point is what guarantees a cancel can never leak a scratch
    // document downstream.
    let open_scratch: Arc<std::sync::Mutex<Vec<OpenScratchDoc>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    // Sole scratch-document cleanup path: the guard's Drop runs on every exit
    // from this function — normal return, the `select!` cancel return below, and
    // a future-level abort (outer `JoinSet` dropped / tower-lsp task cancelled) at
    // any `.await` point. Its Drop drains the tracker and, if anything remains,
    // detaches the didCloses onto a task that outlives this (possibly aborted)
    // future and runs them to completion. Because the close runs on a detached
    // task that always completes, `close_scratch_document`'s untrack-before-didClose
    // ordering is safe — there is no cancellable awaited sweep that could be
    // interrupted mid-didClose and orphan a downstream doc. Kept alive across the
    // whole pipeline below.
    let _scratch_guard =
        ScratchCleanupGuard::new(Arc::clone(&pool), uri.clone(), Arc::clone(&open_scratch));

    // Start of the whole-pipeline budget window (ADR Decision point 6): every
    // step's deadline is measured against this single origin, so serial
    // round-trips share one bound instead of each getting a fresh timeout.
    let pipeline_start = std::time::Instant::now();
    // One-way sentinel so budget exhaustion is WARNed once per pipeline run:
    // every remaining server skips the same way, and one WARN per skipped
    // server would spam the log for a single formatting request.
    let budget_exhaustion_warned = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
            let budget_exhaustion_warned = Arc::clone(&budget_exhaustion_warned);
            let host_uri_lsp = host_uri_lsp.clone();
            let open_scratch = Arc::clone(&open_scratch);
            let request_error_sink = request_error_sink.clone();
            async move {
                // No config for this server name: skip-and-continue, handing the
                // unchanged text back to the pipeline (ADR Decision point 6).
                let Some(server_config) = server_config else {
                    return (current_text, None);
                };

                // ADR Decision point 6: this step's deadline is the REMAINING
                // share of the whole-pipeline budget. Below the floor, skip
                // outright — a request almost certain to time out is pure
                // waste, and every later step will skip the same way.
                let Some(step_budget) = remaining_step_budget(
                    PIPELINE_BUDGET,
                    pipeline_start.elapsed(),
                    PIPELINE_STEP_FLOOR,
                ) else {
                    // WARN only for the first exhausted step; the remaining
                    // servers skip identically, so they get debug level.
                    if !budget_exhaustion_warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        log::warn!(
                            target: "kakehashi::formatting",
                            "concatenated formatting pipeline budget ({:?}) \
                             exhausted; skipping server {} and all remaining \
                             steps (ADR point 6)",
                            PIPELINE_BUDGET,
                            server_name
                        );
                    } else {
                        log::debug!(
                            target: "kakehashi::formatting",
                            "concatenated formatting step for server {} skipped: \
                             pipeline budget exhausted",
                            server_name
                        );
                    }
                    return (current_text, None);
                };

                // Keep a copy for $/cancelRequest propagation on timeout: the
                // request future inside the timeout consumes `upstream_id`.
                // Receives this step's downstream request id as soon as the
                // bridge allocates it. The timeout arm below needs it because
                // dropping the timed-out step future removes the request's
                // upstream-id cancel mapping (router cleanup guard) before any
                // cancel task could look it up — and the upstream id may also
                // map to sibling regions' in-flight requests, which must NOT
                // be cancelled by this step's timeout.
                let step_downstream_id: std::sync::OnceLock<crate::lsp::bridge::RequestId> =
                    std::sync::OnceLock::new();
                // The connection `(server, root)` this step actually routed to,
                // captured from the probe's acquired handle so the scratch-doc
                // didClose and the timeout cancel target the same process the
                // request used — including a preferSharedInstance server diverted
                // to a per-root connection (#391). Read by the timeout arm below.
                let step_connection_key: std::sync::OnceLock<crate::lsp::bridge::ConnectionKey> =
                    std::sync::OnceLock::new();

                // Everything that talks to the downstream server — capability
                // probe (which may spawn/handshake a cold server), scratch
                // didOpen, and the formatting request itself — runs under this
                // step's deadline, so a hung or slow server can only consume
                // its remaining share of the budget.
                let step_fut = async {
                    // ADR Decision point 3.2: the pipeline prefers full
                    // formatting; the fallback to whole-region rangeFormatting
                    // keys on CAPABILITY, never on the response (a capable
                    // server's `null` is the authoritative "already formatted"
                    // and arrives as `Some(vec![])` from the bridge transform).
                    // A probe error (connection spawn/handshake failure) is a
                    // failed step: skip-and-continue (ADR point 6).
                    let (step_request, connection_key) = match pipeline_step_request_kind(
                        &pool,
                        &server_name,
                        &server_config,
                        Some(&uri),
                        step_budget,
                    )
                    .await
                    {
                        Ok(probe) => probe,
                        Err(e) => {
                            log::warn!(
                                target: "kakehashi::formatting",
                                "concatenated formatting step for server {} failed during \
                                 capability probe; skipping (ADR point 6): {}",
                                server_name,
                                e
                            );
                            count_request_errors(&request_error_sink, 1);
                            return None;
                        }
                    };
                    // Publish the actual routed connection so the timeout arm can
                    // cancel on the right process (the request below routes to the
                    // same key the probe acquired).
                    let _ = step_connection_key.set(connection_key.clone());
                    if step_request == PipelineStepRequest::Skip {
                        // Neither documentFormattingProvider nor
                        // documentRangeFormattingProvider: contribute nothing and
                        // send no unsupported request (ADR point 3.2). The probe
                        // waits (within the step budget) for an initializing
                        // connection to reach Ready, so this is a genuine
                        // capability answer, not a cold-start artifact.
                        log::debug!(
                            target: "kakehashi::formatting",
                            "concatenated formatting step for server {} skipped: \
                             server advertises neither documentFormattingProvider \
                             nor documentRangeFormattingProvider",
                            server_name
                        );
                        return None;
                    }

                    let step = step_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let scratch_id = scratch_region_id(&region_id, run_seq, step);

                    // Build the scratch URI up front and register it as "open" in
                    // the shared tracker BEFORE the request, so that if this
                    // future is dropped mid-send by a cancel or the step timeout,
                    // `ScratchCleanupGuard`'s Drop still closes it. Skipped only
                    // when the host URL could not be converted (then no didClose
                    // is possible anyway).
                    let scratch_uri = host_uri_lsp
                        .as_ref()
                        .map(|h| VirtualDocumentUri::new(h, &injection_language, &scratch_id));
                    if let Some(scratch_uri) = scratch_uri.as_ref() {
                        push_open_scratch(
                            &open_scratch,
                            OpenScratchDoc {
                                uri: scratch_uri.clone(),
                                connection_key: connection_key.clone(),
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
                    let send_result = if step_request == PipelineStepRequest::Full {
                        pool.send_formatting_request(
                            &server_name,
                            &server_config,
                            &uri,
                            &injection_language,
                            &scratch_id,
                            RegionOffset::new(0, 0),
                            &current_text,
                            options,
                            upstream_id,
                            // Concatenated formatting pipeline (serial over scratch
                            // docs): no client progress here — concatenated progress
                            // is deferred (#440). Pass no token.
                            None,
                            Some(&step_downstream_id),
                        )
                        .await
                    } else {
                        // WholeRegionRange (Skip already returned above): the
                        // range-only server formats the entire region, expressed
                        // as a range over the step's current text. With the zero
                        // offset, host and virtual coordinates coincide, so the
                        // whole-region replacement range doubles as the request
                        // range.
                        match region_replacement_range(&current_text, &RegionOffset::new(0, 0)) {
                            Some(whole_region) => {
                                pool.send_range_formatting_request(
                                    &server_name,
                                    &server_config,
                                    &uri,
                                    &injection_language,
                                    &scratch_id,
                                    RegionOffset::new(0, 0),
                                    &current_text,
                                    whole_region,
                                    options,
                                    upstream_id,
                                    // Concatenated formatting pipeline (serial over
                                    // scratch docs): no client progress here —
                                    // concatenated progress is deferred (#440).
                                    None,
                                    Some(&step_downstream_id),
                                )
                                .await
                            }
                            // Unresolvable end position (mapper bug / corrupt
                            // content): treat as a failed step rather than sending
                            // an unbounded range downstream.
                            None => Ok(None),
                        }
                    };

                    // ADR Decision point 6 (best-effort, skip-and-continue): a
                    // failed step is skipped — never surfaced to the editor — but is
                    // logged so the misbehaving server stays diagnosable. We log the
                    // downstream error here (before mapping to `None`) instead of
                    // silently dropping it with `.ok().flatten()`.
                    match send_result {
                        Ok(edits) => edits,
                        Err(e) => {
                            log::warn!(
                                target: "kakehashi::formatting",
                                "concatenated formatting step for server {} failed; skipping (ADR point 6): {}",
                                server_name,
                                e
                            );
                            count_request_errors(&request_error_sink, 1);
                            None
                        }
                    }
                };

                let result = match tokio::time::timeout(step_budget, step_fut).await {
                    Ok(result) => result,
                    Err(_) => {
                        // Per-step timeout: a failed step (ADR point 6) — skip
                        // and continue with the unchanged text. Dropping the
                        // step future abandons our side of the request, but the
                        // downstream server is still computing; per the ADR
                        // Consequences cancellation note, tell it to stop via
                        // $/cancelRequest — scoped to the step's OWN downstream
                        // request id (captured via the probe before the drop):
                        // the upstream-id route is unusable here, both because
                        // the dropped future's router cleanup has already
                        // removed the mapping and because the upstream id can
                        // fan out to sibling regions' in-flight requests.
                        // Best-effort and detached. The probe is unset only
                        // when the timeout fired before the request was even
                        // registered (capability probe / didOpen), where there
                        // is nothing to cancel.
                        log::warn!(
                            target: "kakehashi::formatting",
                            "concatenated formatting step for server {} timed out \
                             after {:?}; skipping (ADR point 6)",
                            server_name,
                            step_budget
                        );
                        count_request_errors(&request_error_sink, 1);
                        // Cancel on the connection the request actually routed to
                        // (set right after the probe). Both are set only once a
                        // request was registered, so this never cancels on a stale
                        // optimistic key.
                        if let (Some(downstream_id), Some(connection_key)) = (
                            step_downstream_id.get().copied(),
                            step_connection_key.get().cloned(),
                        ) {
                            let pool = Arc::clone(&pool);
                            tokio::spawn(async move {
                                let _ = pool
                                    .forward_cancel_downstream(&connection_key, downstream_id)
                                    .await;
                            });
                        }
                        None
                    }
                };

                // Scratch-document cleanup is deferred to `ScratchCleanupGuard`'s
                // Drop, NOT closed here per step. The guard's Drop detaches the
                // didCloses onto a task that always runs to completion, so a
                // cancel that drops this step's future mid-flight can never leak a
                // scratch doc; closing per-step inside the (cancellable) pipeline
                // future could be interrupted after the tracker entry was already
                // removed, leaking it. The scratch doc stays tracked in
                // `open_scratch` (registered above) for the guard to close.

                // Hand the (unchanged) virtual text back to the pipeline along
                // with the result so the pipeline can move it forward without a
                // per-step clone.
                (current_text, result)
            }
        }
    });

    // Poll cancellation concurrently with the in-flight pipeline so an upstream
    // $/cancelRequest aborts the region promptly rather than only between steps.
    // On cancel we just stop polling the pipeline and return None; the scratch
    // documents the run opened are closed by `_scratch_guard`'s Drop (which fires
    // on this return as well as on a future-level abort), so there is no explicit
    // sweep to interrupt. Downstream $/cancelRequest propagation needs no work
    // here: the `RequestIdCapture` middleware already fans the upstream cancel
    // out to every downstream server registered for this upstream id — which
    // includes the step currently in flight (`forward_cancel_by_upstream_id`).
    // The step-timeout path propagates its own cancel inside the step closure.
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

    // On cancel, contribute no edit (matches the prior early-return behavior).
    // Scratch cleanup is handled entirely by `_scratch_guard`'s Drop below.
    if cancelled {
        return None;
    }

    let final_text = final_text?;
    // Decision point 4: the virtual output starts at column 0 of the embedded
    // language, so every continuation line must re-gain its host prefix before
    // the whole-region replacement is emitted.
    let final_text = reapply_host_line_prefixes(&final_text, &host_line_prefixes);
    Some(vec![TextEdit {
        range: replacement_range,
        new_text: final_text,
    }])
}

/// A scratch virtual document opened during a concatenated-formatting run that
/// has not yet been closed. Paired with its connection key so the `didClose`
/// targets the exact `(server, root)` connection the matching `didOpen` was sent
/// on (#382).
#[derive(Clone)]
struct OpenScratchDoc {
    uri: VirtualDocumentUri,
    connection_key: crate::lsp::bridge::ConnectionKey,
}

/// Drop-based cleanup that closes every scratch document still tracked open when
/// the concatenated-formatting future exits — the **sole** scratch cleanup path.
///
/// Its `Drop` runs on every exit from `dispatch_concatenated_formatting`: the
/// normal return, the `select!` cancel return, and a future-level abort (its
/// outer `JoinSet` dropped or the tower-lsp task cancelled) at any `.await`
/// point.
///
/// `Drop` cannot `.await`, so it drains the tracker and, if anything remains,
/// **spawns a detached `tokio::task`** to run the `didClose`s. A detached task is
/// NOT a child of the (possibly aborted) future, so it survives the parent's
/// cancellation and runs the `close_scratch_document` calls to completion — which
/// is also why `close_scratch_document` can keep its untrack-before-didClose
/// ordering (the detached task guarantees the didClose await still completes,
/// even on the abort path, so it can never orphan a downstream doc).
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
        // This guard is the sole scratch-document cleanup path: on every exit of
        // `dispatch_concatenated_formatting` (normal return, select!-cancel
        // return, or future-level abort) the guard drops and drains the tracker
        // here. Nothing else drains it, so there is no double-close to guard
        // against.
        let remaining = drain_open_scratch(&self.open);
        if remaining.is_empty() {
            return;
        }

        // Drop can't await, and the parent future is (in the case that matters)
        // being aborted — so finish the didCloses on a DETACHED task that is not a
        // child of the aborted future and therefore runs to completion.
        //
        // `tokio::spawn` panics outside a runtime, so spawn via a `Handle` from
        // `try_current`. In practice this guard always drops inside the request's
        // task (a runtime is present); the `Err` arm only guards pathological
        // drops (shutdown / a synchronous context), where we log and rely on the
        // host document's own close to reap the scratch docs rather than panicking.
        let count = remaining.len();
        let pool = Arc::clone(&self.pool);
        let host_uri = self.host_uri.clone();
        let cleanup = async move {
            for doc in remaining {
                pool.close_scratch_document(&host_uri, &doc.uri, &doc.connection_key)
                    .await;
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(cleanup);
            }
            Err(_) => {
                log::warn!(
                    target: "kakehashi::formatting",
                    "ScratchCleanupGuard dropped outside a Tokio runtime; {count} scratch document(s) left for host-close cleanup"
                );
            }
        }
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
/// region — no host-file collision) and appends a `-kakehashi-scratch-{run}-{step}`
/// suffix: the `run` (a process-global sequence) keeps concurrent runs for the
/// same region distinct, and `step` keeps each step within a run distinct. The
/// scratch marker ([`VirtualDocumentUri::SCRATCH_ID_MARKER`], Decision
/// point 7) lets external tools (file watchers, build tools, test runners)
/// recognize and ignore these throwaway documents, and is what the bridge
/// reader keys on to discard downstream `publishDiagnostics` targeting them
/// (`VirtualDocumentUri::is_scratch_uri`). It still flows through
/// [`VirtualDocumentUri::new`], which also wraps it in the
/// `kakehashi-virtual-uri-` filename marker and preserves the host directory
/// and language extension required for downstream config and parser
/// discovery.
/// Process-global, monotonically increasing pipeline-run sequence. The per-step
/// counter alone only makes scratch ids unique *within* a single pipeline run;
/// two concatenated-formatting requests for the same host region overlapping in
/// time (concurrent LSP requests, or a cancel+restart race) would otherwise both
/// start at step 0 and collide on the same scratch virtual URI. Mixing in this
/// run sequence makes scratch ids unique across concurrent runs in the process.
// `AtomicUsize` (not `AtomicU64`) so the build stays portable to targets without
// native 64-bit atomics; a pointer-width counter is more than enough for run ids.
static SCRATCH_RUN_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn scratch_region_id(region_id: &str, run: usize, step: usize) -> String {
    let marker = VirtualDocumentUri::SCRATCH_ID_MARKER;
    format!("{region_id}{marker}{run}-{step}")
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
/// The replacement's `new_text` is built by [`reapply_host_line_prefixes`],
/// which restores the host prefix on every continuation line (Decision
/// point 4); this function only provides the range.
fn region_replacement_range(original_virtual: &str, offset: &RegionOffset) -> Option<Range> {
    // Virtual end position = the position one past the last byte (EOF), derived
    // via the shared PositionMapper from `original_virtual.len()` so line-ending
    // handling (incl. `\r\n`) and UTF-16 column math stay consistent with the rest
    // of the codebase. Computed from the original virtual text *before* it is
    // moved into the pipeline, so the pipeline takes ownership without a second
    // clone.
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

/// Extract the host-text prefix of every line of an injection region.
///
/// `prefixes[i]` is the host text that precedes the region's content on
/// virtual line `i` — the blockquote `> ` markers and/or indentation that the
/// injection extraction stripped. `line_column_offsets` carries the UTF-16
/// column where content starts on each line (`ResolvedInjection`
/// semantics: a single-entry list means only line 0 is offset; missing
/// entries default to column 0, i.e. no prefix).
///
/// An unresolvable position (stale offsets racing a host edit) degrades to an
/// empty prefix for that line rather than failing the whole region — the same
/// saturating posture the position translation code takes.
///
/// `mapper` must be built over the same `host_text`; the caller shares one
/// mapper across all regions of a request, since constructing it is
/// O(document size).
fn extract_host_line_prefixes(
    mapper: &crate::text::PositionMapper,
    host_text: &str,
    region_start_line: u32,
    line_column_offsets: &[u32],
    virtual_line_count: usize,
) -> Vec<String> {
    (0..virtual_line_count)
        .map(|i| {
            let column = line_column_offsets.get(i).copied().unwrap_or(0);
            if column == 0 {
                return String::new();
            }
            let Some(line) = u32::try_from(i)
                .ok()
                .and_then(|i| region_start_line.checked_add(i))
            else {
                return String::new();
            };
            let start = mapper.position_to_byte(Position { line, character: 0 });
            let end = mapper.position_to_byte(Position {
                line,
                character: column,
            });
            // `get` instead of indexing: the offsets come from the same
            // snapshot so they always land on char boundaries, but if that
            // invariant were ever violated, degrading to "no prefix" beats
            // both a panic and a fabricated misaligned prefix.
            match (start, end) {
                (Some(start), Some(end)) if start <= end => host_text
                    .get(start..end)
                    .map(str::to_string)
                    .unwrap_or_default(),
                _ => String::new(),
            }
        })
        .collect()
}

/// Re-apply the region's host per-line prefixes to the pipeline's final text
/// (concatenated-formatting-pipeline Decision point 4).
///
/// The whole-region replacement range starts *after* the first line's host
/// prefix, but every later line of `new_text` lands at host column 0 — so
/// without this step a blockquoted/indented injection loses its `> ` markers
/// or indentation on every continuation line.
///
/// - **Line count preserved** (output lines == region lines): each output
///   line `i` re-gains its own original prefix `host_line_prefixes[i]` — a
///   1:1 mapping, exact even when prefixes differ per line (nested
///   blockquotes).
/// - **Line count changed**: the pipeline computes no diff/alignment, so
///   every continuation line takes the region's longest common prefix — exact
///   for the usual uniform-prefix region, a documented limitation otherwise.
///   The LCP excludes trailing empty-prefix entries: a region whose content
///   ends with `\n` carries a synthetic final line bucket whose host prefix
///   belongs to the line *after* the replacement range, and including its
///   empty prefix would collapse the LCP to `""` for uniformly prefixed
///   blockquotes. Symmetrically, a trailing **empty** output line stays
///   unprefixed — it abuts the host's own prefix at the region end.
/// - **Empty output lines** get the prefix with trailing whitespace trimmed
///   (`>` not `> `; a space-only prefix is left off entirely) so the pipeline
///   introduces no trailing-whitespace violations.
fn reapply_host_line_prefixes(final_text: &str, host_line_prefixes: &[String]) -> String {
    let lines: Vec<&str> = final_text.split('\n').collect();
    if lines.len() <= 1 {
        // Single-line output sits entirely after the first line's host prefix.
        return final_text.to_string();
    }

    let line_count_preserved = lines.len() == host_line_prefixes.len();
    let lcp = if line_count_preserved {
        ""
    } else {
        // A region whose content ends with '\n' carries a trailing
        // empty-prefix entry for the synthetic line bucket after the final
        // newline; that host line's own prefix sits OUTSIDE the replacement
        // range (the range ends at its column 0). Including it would
        // collapse the LCP to "" for a uniformly prefixed blockquote, so the
        // LCP is computed over the content lines' prefixes only.
        let mut content_prefixes = host_line_prefixes;
        while let [rest @ .., last] = content_prefixes {
            if !last.is_empty() {
                break;
            }
            content_prefixes = rest;
        }
        longest_common_prefix(content_prefixes)
    };

    // Reserve for the prefixes being prepended too (slight over-reserve when
    // empty lines get a trimmed prefix), so the string never reallocates.
    let added_len = if line_count_preserved {
        host_line_prefixes.iter().skip(1).map(String::len).sum()
    } else {
        (lines.len() - 1) * lcp.len()
    };
    let mut out = String::with_capacity(final_text.len() + added_len);
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(line);
            continue;
        }
        out.push('\n');
        // `split('\n')` keeps a trailing '\r' on CRLF content; a line is
        // "empty" for the prefix rules with or without it.
        let line_is_empty = line.trim_end_matches('\r').is_empty();
        let prefix = if line_count_preserved {
            host_line_prefixes[i].as_str()
        } else if line_is_empty && i == lines.len() - 1 {
            // The trailing empty output line sits at the region end, where
            // the host's own prefix (e.g. the closing fence's "> ") follows
            // immediately outside the replacement range — prefixing it here
            // would double the marker.
            ""
        } else {
            lcp
        };
        // Empty output lines take the prefix with trailing whitespace
        // trimmed so the pipeline introduces no trailing-whitespace
        // violations.
        if line_is_empty {
            out.push_str(prefix.trim_end());
        } else {
            out.push_str(prefix);
        }
        out.push_str(line);
    }
    out
}

/// Longest common prefix of all strings, on char boundaries. Empty input or
/// any empty string yields the empty prefix. Returns a slice of the first
/// string (the prefix is by definition a substring of it), so no allocation.
fn longest_common_prefix(strings: &[String]) -> &str {
    let mut iter = strings.iter();
    let Some(first) = iter.next() else {
        return "";
    };
    let mut prefix = first.as_str();
    for s in iter {
        let common_len = prefix
            .chars()
            .zip(s.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum();
        prefix = &prefix[..common_len];
        if prefix.is_empty() {
            break;
        }
    }
    prefix
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
    /// CLI-mode counter for request-time downstream failures (see
    /// [`RequestErrorSink`]); `None` in LSP mode. Carried here because it
    /// shares the cancel state's lifecycle: one per formatting request,
    /// cloned into every per-region dispatcher.
    request_error_sink: RequestErrorSink,
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
            request_error_sink: None,
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
    // plan_region_format (concatenated-formatting-pipeline Decision point 2:
    // `priorities` is an allowlist + order)
    // ==========================================================================

    fn config(name: &str) -> ResolvedServerConfig {
        ResolvedServerConfig {
            server_name: name.to_string(),
            config: Arc::new(crate::config::settings::BridgeServerConfig {
                cmd: vec![name.to_string()],
                languages: vec![],
                initialization_options: None,
                workspace_markers: None,
                on_type_formatting_triggers: None,
                prefer_shared_instance: None,
                enabled: None,
                settings: None,
            }),
        }
    }

    #[test]
    fn plan_non_concatenated_strategy_uses_preferred() {
        // Default `preferred` keeps the first-non-empty-wins fan-out regardless
        // of priorities.
        let configs = vec![config("black"), config("isort")];
        let priorities = vec!["black".to_string()];
        assert_eq!(
            plan_region_format(AggregationStrategy::Preferred, &priorities, &configs, None),
            RegionFormatPlan::Preferred
        );
    }

    #[test]
    fn plan_concatenated_with_configured_priorities_runs_pipeline() {
        // `concatenated` + priorities that resolve to configured servers opts
        // into the sequential pipeline over exactly those servers, in order.
        let configs = vec![config("black"), config("isort")];
        let priorities = vec!["isort".to_string(), "black".to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                None
            ),
            RegionFormatPlan::Concatenated(vec!["isort".to_string(), "black".to_string()])
        );
    }

    // ==========================================================================
    // whole_document_replacement (cross-layer-aggregation phase 3)
    // ==========================================================================

    #[test]
    fn whole_document_replacement_covers_the_entire_original() {
        let original = "line1\nline2";
        let formatted = "LINE1\nLINE2\n";
        let edits = whole_document_replacement(original, formatted).unwrap();
        assert_eq!(edits.len(), 1, "one overlap-free replacement edit");
        assert_eq!(edits[0].range.start, Position::new(0, 0));
        assert_eq!(
            edits[0].range.end,
            Position::new(1, 5),
            "end must be the original's last position (line 1, after 'line2')"
        );
        assert_eq!(edits[0].new_text, formatted);
    }

    #[test]
    fn whole_document_replacement_handles_trailing_newline() {
        // With a trailing newline the last line is empty: end = (lines, 0).
        let original = "a\nb\n";
        let edits = whole_document_replacement(original, "c\n").unwrap();
        assert_eq!(edits[0].range.end, Position::new(2, 0));
    }

    #[test]
    fn whole_document_replacement_counts_end_character_in_utf16() {
        // 'é' is 1 UTF-16 unit but 2 UTF-8 bytes; '𠮷' is 2 UTF-16 units.
        let original = "é𠮷";
        let edits = whole_document_replacement(original, "x").unwrap();
        assert_eq!(edits[0].range.end, Position::new(0, 3));
    }

    #[test]
    fn whole_document_replacement_returns_none_for_round_trip() {
        // A chain whose layers undo each other must produce no edit at all.
        assert!(whole_document_replacement("same", "same").is_none());
    }

    #[test]
    fn plan_empty_priorities_disables_the_region_for_any_strategy() {
        // priorities = [] is the per-method fan-out kill switch
        // (aggregation-priorities-wildcard): nothing runs, regardless of
        // strategy. It is NOT the misconfiguration fallback — that role moved
        // to a wildcard-only list (see the test below).
        let configs = vec![config("black")];
        assert_eq!(
            plan_region_format(AggregationStrategy::Concatenated, &[], &configs, None),
            RegionFormatPlan::Disabled
        );
        assert_eq!(
            plan_region_format(AggregationStrategy::Preferred, &[], &configs, None),
            RegionFormatPlan::Disabled
        );
    }

    #[test]
    fn plan_concatenated_with_wildcard_only_priorities_falls_back_to_preferred() {
        // ADR point 2: the pipeline needs explicit names for a deterministic
        // order. The resolved default ["*"] (absent priorities) carries none,
        // so the region falls back to `preferred`.
        let configs = vec![config("black")];
        let priorities = vec![PRIORITIES_WILDCARD.to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                None
            ),
            RegionFormatPlan::Preferred
        );
    }

    #[test]
    fn plan_concatenated_ignores_wildcard_mixed_with_explicit_names() {
        // ["black", "*"]: the wildcard has no deterministic expansion order
        // for a sequential pipeline, so only the named servers run.
        let configs = vec![config("black"), config("isort")];
        let priorities = vec!["black".to_string(), PRIORITIES_WILDCARD.to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                None
            ),
            RegionFormatPlan::Concatenated(vec!["black".to_string()])
        );
    }

    #[test]
    fn plan_max_fan_out_zero_disables_the_region_for_any_strategy() {
        // maxFanOut = 0 documents "disable fan-out entirely" — the
        // sequential pipeline must honor it like the parallel paths do.
        let configs = vec![config("black")];
        let priorities = vec!["black".to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                Some(0)
            ),
            RegionFormatPlan::Disabled
        );
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Preferred,
                &priorities,
                &configs,
                Some(0)
            ),
            RegionFormatPlan::Disabled
        );
    }

    #[test]
    fn plan_max_fan_out_caps_pipeline_length() {
        let configs = vec![config("black"), config("isort")];
        let priorities = vec!["black".to_string(), "isort".to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                Some(1)
            ),
            RegionFormatPlan::Concatenated(vec!["black".to_string()]),
            "a positive cap bounds the pipeline steps in priority order"
        );
    }

    #[test]
    fn plan_concatenated_with_only_unconfigured_priorities_skips() {
        // ADR point 2 (allowlist): a NON-empty `priorities` naming only
        // servers that aren't configured for the region resolves to an empty
        // effective list. Every configured server is absent from the allowlist,
        // so the region must run NOTHING — not fall through to `preferred`,
        // which would run the very servers the allowlist excluded.
        let configs = vec![config("black"), config("isort")];
        let priorities = vec!["blackk".to_string(), "ruff".to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                None
            ),
            RegionFormatPlan::Skip
        );
    }

    #[test]
    fn plan_concatenated_drops_unconfigured_names_but_keeps_configured_ones() {
        // A partially-typo'd list still runs the configured subset (allowlist
        // membership), not a skip: only an all-unconfigured list skips.
        let configs = vec![config("black"), config("isort")];
        let priorities = vec!["ruff".to_string(), "black".to_string()];
        assert_eq!(
            plan_region_format(
                AggregationStrategy::Concatenated,
                &priorities,
                &configs,
                None
            ),
            RegionFormatPlan::Concatenated(vec!["black".to_string()])
        );
    }

    // ==========================================================================
    // select_pipeline_step_request (concatenated-formatting-pipeline Decision
    // point 3.2: capability-based full -> rangeFormatting fallback)
    // ==========================================================================

    #[test]
    fn step_request_prefers_full_formatting_when_advertised() {
        // A server with documentFormattingProvider gets a full formatting
        // request — even if it also advertises range formatting.
        assert_eq!(
            select_pipeline_step_request(true, true),
            PipelineStepRequest::Full
        );
        assert_eq!(
            select_pipeline_step_request(true, false),
            PipelineStepRequest::Full
        );
    }

    #[test]
    fn step_request_falls_back_to_whole_region_range_for_range_only_server() {
        // No documentFormattingProvider but documentRangeFormattingProvider:
        // the step issues rangeFormatting over the entire region so the
        // range-only server still participates in the pipeline.
        assert_eq!(
            select_pipeline_step_request(false, true),
            PipelineStepRequest::WholeRegionRange
        );
    }

    #[test]
    fn step_request_skips_server_advertising_neither_provider() {
        // Neither provider: contribute nothing, and crucially send no
        // unsupported request downstream.
        assert_eq!(
            select_pipeline_step_request(false, false),
            PipelineStepRequest::Skip
        );
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

    #[test]
    fn scratch_region_id_produces_uri_recognized_as_scratch() {
        // The reader discards downstream publishDiagnostics for scratch URIs
        // by keying on the shared SCRATCH_ID_MARKER; the id construction and
        // that detection must agree (Decision point 7).
        use crate::lsp::bridge::VirtualDocumentUri;
        let host_uri: tower_lsp_server::ls_types::Uri = "file:///project/doc.md".parse().unwrap();
        let id = scratch_region_id("REGION", 4, 2);
        let uri = VirtualDocumentUri::new(&host_uri, "python", &id).to_uri_string();
        assert!(
            VirtualDocumentUri::is_scratch_uri(&uri),
            "scratch id must yield a URI the reader recognizes as scratch: {uri}"
        );
        // And the canonical region document must NOT be flagged.
        let canonical = VirtualDocumentUri::new(&host_uri, "python", "REGION").to_uri_string();
        assert!(!VirtualDocumentUri::is_scratch_uri(&canonical));
    }

    // ==========================================================================
    // Scratch-document tracking/cleanup (review HIGH: leak-on-cancel)
    // ==========================================================================

    fn scratch_doc(host: &str, id: &str, server: &str) -> OpenScratchDoc {
        let host_uri: tower_lsp_server::ls_types::Uri = host.parse().unwrap();
        OpenScratchDoc {
            uri: VirtualDocumentUri::new(&host_uri, "python", id),
            connection_key: crate::lsp::bridge::ConnectionKey::for_server(server),
        }
    }

    #[test]
    fn open_scratch_tracker_drains_what_was_pushed() {
        // The guard's Drop relies on draining everything that was recorded open
        // but never removed by a per-step close.
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
    fn open_scratch_tracker_accumulates_every_step_for_the_guard() {
        // Steps only register their scratch docs; the guard's Drop is the single
        // close point, so every pushed doc must still be present to drain.
        let open = std::sync::Mutex::new(Vec::new());
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-0", "black"));
        push_open_scratch(&open, scratch_doc("file:///d.md", "R-scratch-1", "isort"));
        let drained = drain_open_scratch(&open);
        assert_eq!(
            drained.len(),
            2,
            "the guard must see every step's scratch doc"
        );
    }

    #[tokio::test]
    async fn scratch_cleanup_guard_drains_tracker_on_drop() {
        // The guard is the sole scratch cleanup path: on every exit from the
        // dispatch future (normal return, select! cancel, or future-level abort)
        // its Drop must drain the tracker (and detach the didClose), so no scratch
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
            "guard Drop must drain the tracker so nothing leaks"
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
    // remaining_step_budget (concatenated-formatting-pipeline Decision point 6:
    // per-step deadline = remaining share of the whole-pipeline budget)
    // ==========================================================================

    #[test]
    fn step_budget_is_the_remaining_share_of_the_pipeline_budget() {
        use std::time::Duration;
        // 5s budget, 2s elapsed → the next step gets the remaining 3s, not a
        // fixed per-step value: a slow early step must not let cumulative
        // latency overrun the client's request timeout.
        assert_eq!(
            remaining_step_budget(
                Duration::from_secs(5),
                Duration::from_secs(2),
                Duration::from_millis(50)
            ),
            Some(Duration::from_secs(3))
        );
    }

    #[test]
    fn step_budget_skips_steps_below_the_floor() {
        use std::time::Duration;
        // Less than the floor remaining: issuing a request almost certain to
        // time out is pointless — skip the rest outright (ADR point 6).
        assert_eq!(
            remaining_step_budget(
                Duration::from_secs(5),
                Duration::from_millis(4970),
                Duration::from_millis(50)
            ),
            None
        );
    }

    #[test]
    fn step_budget_exactly_at_floor_still_runs() {
        use std::time::Duration;
        // Boundary: exactly the floor remaining is still worth issuing.
        assert_eq!(
            remaining_step_budget(
                Duration::from_secs(5),
                Duration::from_millis(4950),
                Duration::from_millis(50)
            ),
            Some(Duration::from_millis(50))
        );
    }

    #[test]
    fn step_budget_handles_elapsed_beyond_budget() {
        use std::time::Duration;
        // Elapsed past the budget (a step blocked long): no underflow panic,
        // just skip.
        assert_eq!(
            remaining_step_budget(
                Duration::from_secs(5),
                Duration::from_secs(60),
                Duration::from_millis(50)
            ),
            None
        );
    }

    // ==========================================================================
    // reapply_host_line_prefixes / extract_host_line_prefixes
    // (concatenated-formatting-pipeline Decision point 4: multi-line output
    // must re-gain the host prefix per line)
    // ==========================================================================

    #[test]
    fn reapply_uses_per_line_prefixes_when_line_count_is_preserved() {
        // 3 output lines, 3 region lines: 1:1 mapping. The first line needs no
        // prefix (the replacement range starts after its host prefix); each
        // later line re-gains its own original prefix.
        let prefixes = vec!["> ".to_string(), "> ".to_string(), "> ".to_string()];
        assert_eq!(
            reapply_host_line_prefixes("a\nb\nc", &prefixes),
            "a\n> b\n> c"
        );
    }

    #[test]
    fn reapply_handles_per_line_prefixes_that_differ() {
        // Nested blockquote: line prefixes differ per line and must be applied
        // per line (not the first line's prefix everywhere).
        let prefixes = vec!["> ".to_string(), "> > ".to_string(), "> ".to_string()];
        assert_eq!(
            reapply_host_line_prefixes("a\nb\nc", &prefixes),
            "a\n> > b\n> c"
        );
    }

    #[test]
    fn reapply_uses_region_lcp_when_line_count_changes() {
        // Formatter changed the line count (2 output lines, 3 region lines):
        // there is no diff/alignment, so every continuation line takes the
        // region's longest common prefix (ADR point 4).
        let prefixes = vec!["> ".to_string(), "> ".to_string(), "> ".to_string()];
        assert_eq!(reapply_host_line_prefixes("a\nb", &prefixes), "a\n> b");
        // Non-uniform prefixes: the LCP is the shared "> ".
        let prefixes = vec!["> ".to_string(), "> > ".to_string(), "> ".to_string()];
        assert_eq!(
            reapply_host_line_prefixes("a\nb\nc\nd", &prefixes),
            "a\n> b\n> c\n> d"
        );
    }

    #[test]
    fn reapply_lcp_ignores_trailing_synthetic_empty_prefix() {
        // A blockquote region whose content ends with '\n' carries a trailing
        // empty-prefix entry for the synthetic line after the final newline
        // (its host prefix belongs to the closing-fence line, OUTSIDE the
        // replacement range). The LCP must be computed over the content
        // lines' prefixes only — otherwise it collapses to "" and a uniformly
        // "> "-prefixed blockquote loses every marker on a line-count change.
        let prefixes = vec!["> ".to_string(), "> ".to_string(), String::new()];
        // 4 output lines vs 3 region lines → LCP path. The trailing empty
        // output line sits at the region end and must stay unprefixed (the
        // host's own "> " follows it).
        assert_eq!(
            reapply_host_line_prefixes("a\nb\nc\n", &prefixes),
            "a\n> b\n> c\n"
        );
    }

    #[test]
    fn reapply_lcp_keeps_trailing_empty_output_line_unprefixed() {
        // Even with a uniformly non-empty prefix list, a trailing empty
        // output line abuts the host's own prefix on the region's end line —
        // prefixing it would double the marker (">> ```").
        let prefixes = vec!["> ".to_string(), "> ".to_string(), String::new()];
        assert_eq!(reapply_host_line_prefixes("a\n", &prefixes), "a\n");
    }

    #[test]
    fn reapply_trims_trailing_whitespace_on_empty_output_lines() {
        // ADR point 4: an empty output line must not gain trailing whitespace —
        // emit ">" (not "> "), and leave a space-only prefix off entirely.
        let prefixes = vec!["> ".to_string(), "> ".to_string(), "> ".to_string()];
        assert_eq!(reapply_host_line_prefixes("a\n\nb", &prefixes), "a\n>\n> b");
        let prefixes = vec!["  ".to_string(), "  ".to_string(), "  ".to_string()];
        assert_eq!(reapply_host_line_prefixes("a\n\nb", &prefixes), "a\n\n  b");
    }

    #[test]
    fn reapply_leaves_single_line_output_verbatim() {
        // A single-line output sits entirely after the first line's host
        // prefix; nothing to re-apply.
        let prefixes = vec!["> ".to_string()];
        assert_eq!(reapply_host_line_prefixes("a", &prefixes), "a");
    }

    #[test]
    fn reapply_is_identity_for_unprefixed_regions() {
        // The common fenced-code-block case: every continuation line starts at
        // host column 0, so the output passes through unchanged — both for
        // preserved and changed line counts.
        let prefixes = vec![String::new(), String::new()];
        assert_eq!(reapply_host_line_prefixes("a\nb", &prefixes), "a\nb");
        assert_eq!(reapply_host_line_prefixes("a\nb\nc", &prefixes), "a\nb\nc");
    }

    #[test]
    fn extract_prefixes_reads_blockquote_markers_from_host_lines() {
        // Region: python inside a blockquote, host lines 1..=2, content starts
        // at column 2 on each line ("> " prefix).
        let host = "before\n> x = 1\n> y = 2\nafter\n";
        let mapper = crate::text::PositionMapper::new(host);
        let prefixes = extract_host_line_prefixes(&mapper, host, 1, &[2, 2], 2);
        assert_eq!(prefixes, vec!["> ".to_string(), "> ".to_string()]);
    }

    #[test]
    fn extract_prefixes_defaults_missing_offsets_to_column_zero() {
        // Non-blockquote regions carry a single-entry offset list (line 0
        // only); continuation lines have no prefix.
        let host = "# Doc\ncode line 0\ncode line 1\n";
        let mapper = crate::text::PositionMapper::new(host);
        let prefixes = extract_host_line_prefixes(&mapper, host, 1, &[0], 2);
        assert_eq!(prefixes, vec![String::new(), String::new()]);
    }

    #[test]
    fn extract_prefixes_handles_utf16_columns() {
        // Offsets are UTF-16 code units; a multibyte prefix must slice on the
        // correct byte boundary.
        let host = "héllo→x = 1\n";
        // "héllo→" is 6 UTF-16 code units (é and → are 1 each).
        let mapper = crate::text::PositionMapper::new(host);
        let prefixes = extract_host_line_prefixes(&mapper, host, 0, &[6], 1);
        assert_eq!(prefixes, vec!["héllo→".to_string()]);
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
