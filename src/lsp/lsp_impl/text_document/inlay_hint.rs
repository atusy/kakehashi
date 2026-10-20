//! Inlay hint methods for Kakehashi.
//!
//! `textDocument/inlayHint` walks the resolved layer order
//! (cross-layer-aggregation): the virt layer bridges every injection region
//! the requested range intersects and merges their hints in document order,
//! the host layer (host-document-bridge) bridges
//! the host document itself with the real URI and its coordinates verbatim.
//! Either layer re-keys label-part commands for `workspace/executeCommand`
//! routing and, when the producer advertises `inlayHint/resolve` (or its
//! payload occupies the reserved key), envelopes `data` for resolve routing.
//! The first layer producing a non-empty result wins (`preferred`).
//!
//! `inlayHint/resolve` gates the echoed envelope on freshness (content
//! version, and for the virt layer the live region offset and contiguity),
//! forwards to the exact producing connection, and re-checks the content
//! version and host incarnation after the response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{InlayHint, InlayHintParams, NumberOrString, Range, Uri};

use super::super::Kakehashi;
use super::super::region_offset::resolve_region_offset;
use crate::lsp::aggregation::server::{
    FanInResult, FanOutTask, HostFanOutTask, dispatch_host_preferred, dispatch_preferred,
    dispatch_preferred_with_tokens, mint_region_progress_source,
};
use crate::lsp::bridge::{
    ClientProgressAggregator, ClientProgressDeregisterGuard, HostDocument,
    InlayHintDocumentRevision, InlayHintEnvelope, envelope_host_inlay_hints,
    extract_inlay_hint_envelope,
};
use crate::lsp::current_upstream_id;
use crate::lsp::lsp_impl::bridge_context::{RangeRequestContext, parse_host_verbatim};

const METHOD: &str = "textDocument/inlayHint";

impl Kakehashi {
    pub(crate) async fn inlay_hint_impl(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        // Move (not clone) the token out — `params` is consumed below.
        let work_done_token = params.work_done_progress_params.work_done_token;
        let lsp_uri = params.text_document.uri;
        let range = params.range;
        let virt = self.inlay_hint_virt_layer(&lsp_uri, range, work_done_token);
        let host = self.inlay_hint_host_layer(&lsp_uri, raw_params);
        self.walk_layer_futures(
            &lsp_uri,
            METHOD,
            METHOD,
            virt,
            host,
            std::future::ready(Ok(None)),
            |hints: &Vec<InlayHint>| !hints.is_empty(),
        )
        .await
    }

    async fn inlay_hint_host_layer(
        &self,
        lsp_uri: &Uri,
        raw_params: serde_json::Value,
    ) -> Result<Option<Vec<InlayHint>>> {
        let Some(ctx) = self.resolve_host_bridge_context(lsp_uri, METHOD) else {
            return Ok(None);
        };
        // The stamps travel with the text they were read with, and a reply
        // synchronized under another lifetime is refused below: a close and
        // reopen racing the request would otherwise answer for the old text
        // under the reopened document's incarnation, and a reopened document
        // restarts its content version, so both stamps could pass.
        let (expected_incarnation, content_version) = (ctx.incarnation, ctx.content_version);
        let revision = crate::lsp::bridge::HostRevision {
            incarnation: expected_incarnation,
            content_version,
        };
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(ctx.upstream_request_id.as_ref());
        let pool = self.bridge.pool_arc();
        let fan_in = dispatch_host_preferred(
            &ctx,
            pool,
            move |t: HostFanOutTask| {
                let params = raw_params.clone();
                async move {
                    let raw = t
                        .pool
                        .send_host_raw_request(
                            &t.server_name,
                            &t.server_config,
                            &HostDocument {
                                uri: &t.uri,
                                language_id: &t.language_id,
                                text: &t.text,
                                revision: Some(revision),
                            },
                            METHOD,
                            params,
                            t.upstream_id,
                        )
                        .await?;
                    let Some(raw) = raw else {
                        return Ok(None);
                    };
                    if raw.incarnation != expected_incarnation {
                        log::debug!(
                            target: "kakehashi::bridge",
                            "textDocument/inlayHint (host): {} was reopened while {} answered; discarding hints computed on the closed text",
                            t.uri,
                            t.server_name
                        );
                        return Ok(None);
                    }
                    let Some(hints) = parse_host_verbatim::<Vec<InlayHint>>(raw.value) else {
                        return Ok(None);
                    };
                    Ok(Some(HostInlayHints {
                        hints,
                        server_name: t.server_name,
                        host_uri: t.uri.to_string(),
                        incarnation: Some(expected_incarnation),
                        content_version,
                        connection_generation: raw.connection_generation,
                        server_resolves: raw.handle.has_capability("inlayHint/resolve"),
                        connection_key: raw.handle.key().clone(),
                    }))
                }
            },
            |opt| matches!(opt, Some(v) if !v.hints.is_empty()),
            cancel_rx,
        )
        .await;
        self.host_layer_result(fan_in, &ctx, METHOD, |won| {
            won.map(HostInlayHints::into_enveloped_hints)
        })
        .await
    }

    /// Virt layer: bridge every injection region the requested range
    /// intersects, each with the range clipped to it, and merge the hints in
    /// document order. Editors ask for the visible or the whole document's
    /// range, which usually starts in host text before any injection.
    async fn inlay_hint_virt_layer(
        &self,
        lsp_uri: &Uri,
        range: Range,
        client_progress_token: Option<NumberOrString>,
    ) -> Result<Option<Vec<InlayHint>>> {
        // Read before the preamble snapshots the document, so the stamp can
        // only be older than the content the hints were computed on, never
        // newer: an edit landing in between makes every hint of this
        // response fail soft on resolve until the editor's next request,
        // which follows an edit anyway. A reopen in that window is caught by
        // the region id, which the reopened document re-tracks.
        let Some(revision) = url::Url::parse(lsp_uri.as_str()).ok().and_then(|uri| {
            self.documents
                .get(&uri)
                .map(|doc| crate::lsp::bridge::HostRevision {
                    incarnation: doc.incarnation(),
                    content_version: doc.content_version(),
                })
        }) else {
            return Ok(None);
        };
        // A region's per-line stripped prefixes (a blockquote's `> `) carry
        // no embedded text: each endpoint is moved to where its line's text
        // starts, and a region the range meets only inside prefixes is not
        // asked at all, as translation would otherwise saturate the range
        // onto text it does not cover.
        let contexts: Vec<_> = self
            .resolve_bridge_contexts_for_all_overlapping_regions(lsp_uri, range, METHOD)
            .await
            .into_iter()
            .filter_map(|mut ctx| {
                let resolved = &ctx.document.resolved;
                let offset = crate::lsp::bridge::RegionOffset::with_per_line_offsets(
                    resolved.region.line_range.start,
                    resolved.line_column_offsets.clone(),
                );
                ctx.range = clip_to_line_text(ctx.range, &offset)?;
                Some(ctx)
            })
            .collect();
        let Some(first) = contexts.first() else {
            return Ok(None);
        };

        // Every region shares this request's upstream id, so one cancel
        // subscription covers the whole walk; the per-region dispatches get
        // none of their own. Registry cleanup is left to the layer race's
        // request-wide sweep, as the host layer shares the id.
        let (cancel_rx, _cancel_guard) =
            self.subscribe_cancel(first.document.upstream_request_id.as_ref());

        // The editor's work-done progress spans the whole request: ONE
        // aggregator and ONE teardown guard, with each region's tracked
        // source minted into it, so the aggregator's winner rule shows one
        // `Begin → … → End` and the synthetic terminal `End` fires only after
        // every region settled (ls-bridge-client-progress). `None` when the
        // request carries no `workDoneToken`.
        let pool = self.bridge.pool_arc();
        let shared_progress = client_progress_token.map(|client_token| {
            (
                Arc::new(Mutex::new(ClientProgressAggregator::new(client_token))),
                Arc::clone(pool.client_progress_registry()),
            )
        });
        let mut minted = Vec::new();
        let regions: Vec<_> = contexts
            .into_iter()
            .map(|ctx| {
                let tokens = shared_progress.as_ref().and_then(|(aggregator, registry)| {
                    mint_region_progress_source(&ctx.document, registry, aggregator)
                });
                if let Some(tokens) = &tokens {
                    minted.extend(tokens.values().cloned());
                }
                (ctx, tokens)
            })
            .collect();
        let progress_guard = shared_progress.map(|(aggregator, registry)| {
            ClientProgressDeregisterGuard::new(registry, minted, aggregator, pool.upstream_tx())
        });

        // The regions are independent documents, so they are asked at once;
        // `join_all` keeps region order for the merge.
        let walk = futures::future::join_all(
            regions
                .into_iter()
                .map(|(ctx, tokens)| self.dispatch_virt_region_inlay_hint(ctx, tokens, revision)),
        );
        let result = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => FanInResult::Cancelled,
                results = walk => merge_region_results(results, |hint: &InlayHint| hint.position),
            },
            None => merge_region_results(walk.await, |hint: &InlayHint| hint.position),
        };
        // Every region settled (or the walk was cancelled and dropped them):
        // end the editor's progress now, before the no-result log, in the
        // order a single dispatch tears its own guard down.
        drop(progress_guard);

        result
            .handle(&self.notifier(), "inlay hint", None, Ok)
            .await
    }

    /// Fan `textDocument/inlayHint` out across the servers bridging ONE
    /// injection region (`preferred`). Cancellation and the client-progress
    /// teardown are held by the caller for the whole multi-region walk;
    /// `progress_tokens` routes this region's tracked source into the
    /// request's shared aggregator.
    async fn dispatch_virt_region_inlay_hint(
        &self,
        ctx: RangeRequestContext,
        progress_tokens: Option<HashMap<String, NumberOrString>>,
        revision: crate::lsp::bridge::HostRevision,
    ) -> FanInResult<Option<Vec<InlayHint>>> {
        let pool = self.bridge.pool_arc();
        let range = ctx.range;
        let send = |t: FanOutTask| async move {
            t.pool
                .send_inlay_hint_request(
                    &t.server_name,
                    &t.server_config,
                    &t.uri,
                    range,
                    t.region_end(),
                    &t.injection_language,
                    &t.region_id,
                    t.offset,
                    &t.virtual_content,
                    revision,
                    t.upstream_id,
                    t.client_progress_token,
                )
                .await
        };
        let is_nonempty = |opt: &Option<Vec<InlayHint>>| matches!(opt, Some(v) if !v.is_empty());
        match progress_tokens {
            Some(tokens) => {
                dispatch_preferred_with_tokens(&ctx.document, pool, send, is_nonempty, None, tokens)
                    .await
            }
            None => dispatch_preferred(&ctx.document, pool, send, is_nonempty, None).await,
        }
    }

    pub(crate) async fn inlay_hint_resolve_impl(&self, hint: InlayHint) -> Result<InlayHint> {
        let Some(envelope) = extract_inlay_hint_envelope(&hint) else {
            return Ok(hint);
        };
        // One parse serves every gate below. An unparsable host URI can only
        // be a mangled echo and fails soft like any other stale envelope.
        let Ok(host_url) = url::Url::parse(&envelope.host_uri) else {
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: envelope host URI {:?} does not parse; returning hint unresolved",
                envelope.host_uri
            );
            return Ok(hint);
        };
        // Every fail-soft exit below is the steady state of a hint the editor
        // kept past an edit, so it logs at debug like the sibling gates do.
        // Lifetime first: a hint from a closed and reopened document (whose
        // revision can match the reopened one's) would otherwise park on the
        // reopened lifetime's parse only to be refused after the reply.
        if !self.host_incarnation_is_current(&host_url, envelope.incarnation) {
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: {} was reopened since the hint was produced; returning hint unresolved",
                envelope.host_uri
            );
            return Ok(hint);
        }
        if !self.inlay_hint_content_is_fresh(&host_url, &envelope) {
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: {} was revised since the hint was produced; returning hint unresolved",
                envelope.host_uri
            );
            return Ok(hint);
        }
        let region_end = if envelope.is_host_layer() {
            None
        } else {
            // `didChange` clears the tree and reparses off-ingress; a resolve
            // issued in that window would find no snapshot and fail soft as a
            // stale region. Wait (bounded) for the current parse before
            // rebuilding the region.
            self.wait_for_resolve_parse(&host_url).await;
            let Some((offset, region_end, contiguous, live_language)) =
                self.inlay_hint_region_geometry(&host_url, &envelope)
            else {
                log::debug!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve: region {} is stale; returning hint unresolved",
                    envelope.region_id
                );
                return Ok(hint);
            };
            if !inlay_hint_region_is_resolvable(&envelope, &offset, contiguous, &live_language) {
                log::debug!(
                    target: "kakehashi::bridge",
                    "inlayHint/resolve: region {} moved or is no longer contiguous; returning hint unresolved",
                    envelope.region_id
                );
                return Ok(hint);
            }
            Some(region_end)
        };

        // Kept for the post-response check; the gates above return `hint`
        // itself, so only a resolve that is actually dispatched pays for it.
        let unresolved = hint.clone();
        let settings = self.settings_manager.load_settings();
        let pool = self.bridge.pool_arc();
        let upstream_id = current_upstream_id();
        let (cancel_rx, _cancel_guard) = self.subscribe_cancel(upstream_id.as_ref());
        let sweep_id = upstream_id.clone();
        let read_host = |uri: &url::Url| self.host_resolve_snapshot(uri);
        let dispatch =
            pool.dispatch_inlay_hint_resolve(hint, &settings, upstream_id, region_end, &read_host);
        let _sweep = crate::lsp::lsp_impl::bridge_context::UpstreamRegistrySweepGuard::new(
            std::sync::Arc::clone(&pool),
            sweep_id,
        );
        let resolved = match cancel_rx {
            Some(rx) => tokio::select! {
                biased;
                _ = rx => Err(tower_lsp_server::jsonrpc::Error::request_cancelled()),
                hint = dispatch => Ok(hint),
            },
            None => Ok(dispatch.await),
        }?;

        // A later didChange/didClose is allowed to proceed once the resolve was
        // enqueued. Revalidate after the response so lazy edits/locations
        // computed against the content this resolve observed are not
        // surfaced into a revised or reopened document. The region is
        // deliberately not rebuilt a second time (an injection walk over the
        // whole tree): a content edit advances the version, a reopen changes
        // the incarnation, and a settings/query reload that could re-track
        // the region invalidates every parse, which advances the version as
        // well (`Document::invalidate_parse`), so the two stamps cover every
        // way the geometry can move.
        if !self.inlay_hint_envelope_is_fresh(&host_url, &envelope, &pool) {
            log::debug!(
                target: "kakehashi::bridge",
                "inlayHint/resolve: {} was revised or reopened while resolving; returning hint unresolved",
                envelope.host_uri
            );
            return Ok(unresolved);
        }
        Ok(resolved)
    }

    fn inlay_hint_envelope_is_fresh(
        &self,
        host_url: &url::Url,
        envelope: &InlayHintEnvelope,
        pool: &crate::lsp::bridge::LanguageServerPool,
    ) -> bool {
        self.inlay_hint_content_is_fresh(host_url, envelope)
            && envelope
                .incarnation
                .is_some_and(|expected| pool.current_host_incarnation(host_url) == Some(expected))
    }

    fn inlay_hint_content_is_fresh(
        &self,
        host_url: &url::Url,
        envelope: &InlayHintEnvelope,
    ) -> bool {
        envelope.content_version.is_some_and(|expected| {
            self.documents
                .get(host_url)
                .is_some_and(|document| document.content_version() == expected)
        })
    }

    fn inlay_hint_region_geometry(
        &self,
        host_url: &url::Url,
        envelope: &InlayHintEnvelope,
    ) -> Option<(
        crate::lsp::bridge::RegionOffset,
        tower_lsp_server::ls_types::Position,
        bool,
        String,
    )> {
        resolve_region_offset(
            &self.documents,
            &self.language,
            &self.bridge,
            host_url,
            &envelope.region_id,
        )
    }
}

/// Whether the live region still matches what the envelope was minted from:
/// contiguous, at the same offset, and of the same injection language. The
/// language is client-echoed data that names the virtual document the
/// request goes to; an empty or different one would address a document the
/// producer never opened (and trips the virtual-URI invariant in debug
/// builds), so it fails soft here with the rest.
fn inlay_hint_region_is_resolvable(
    envelope: &InlayHintEnvelope,
    offset: &crate::lsp::bridge::RegionOffset,
    contiguous: bool,
    live_language: &str,
) -> bool {
    contiguous
        && *offset == crate::lsp::bridge::RegionOffset::from(&envelope.offset)
        && envelope.injection_language == live_language
}

/// Move each endpoint of a host `range` inside a region to where its line's
/// embedded text starts, past the line's stripped prefix (`offset`'s
/// per-line column). `None` when nothing but prefix bytes remains: a
/// non-empty range covering only a prefix, or a caret inside one.
fn clip_to_line_text(range: Range, offset: &crate::lsp::bridge::RegionOffset) -> Option<Range> {
    let snap = |position: tower_lsp_server::ls_types::Position| {
        let text_start = position
            .line
            .checked_sub(offset.line())
            .map_or(0, |virtual_line| offset.column_for_line(virtual_line));
        tower_lsp_server::ls_types::Position {
            character: position.character.max(text_start),
            ..position
        }
    };
    let clipped = Range {
        start: snap(range.start),
        end: snap(range.end),
    };
    let emptied = clipped.start == clipped.end && (range.start != range.end || clipped != range);
    (!emptied).then_some(clipped)
}

/// Merge the per-region fan-in results of one request in document order.
///
/// Any region that answered makes the merge an answer, with every answering
/// region's hints. Only when none answered does it report no result (once,
/// counting every region's errors), so a request over several regions logs
/// as one request. A single region's result passes through unchanged.
///
/// When several regions answered, the hints are stably sorted by `position`:
/// regions come in document order of their starts, but overlapping
/// same-depth regions interleave, so appending alone would not keep the
/// hints in document order. Hints at one position keep their arrival order.
fn merge_region_results<T, K: Ord>(
    results: Vec<FanInResult<Option<Vec<T>>>>,
    position: impl Fn(&T) -> K,
) -> FanInResult<Option<Vec<T>>> {
    let mut merged: Option<Vec<T>> = None;
    let mut answered = 0usize;
    let mut errors = None;
    for result in results {
        match result {
            FanInResult::Done(Some(items)) => {
                answered += 1;
                merged.get_or_insert_with(Vec::new).extend(items);
            }
            FanInResult::Done(None) => {}
            FanInResult::NoResult { errors: e } => *errors.get_or_insert(0) += e,
            FanInResult::Cancelled => return FanInResult::Cancelled,
        }
    }
    if answered > 1
        && let Some(items) = merged.as_mut()
    {
        items.sort_by_key(|item| position(item));
    }
    match (merged, errors) {
        (Some(items), _) => FanInResult::Done(Some(items)),
        (None, Some(errors)) => FanInResult::NoResult { errors },
        (None, None) => FanInResult::Done(None),
    }
}

struct HostInlayHints {
    hints: Vec<InlayHint>,
    server_name: String,
    host_uri: String,
    incarnation: Option<u64>,
    content_version: u64,
    connection_generation: u64,
    server_resolves: bool,
    connection_key: crate::lsp::bridge::ConnectionKey,
}

impl HostInlayHints {
    fn into_enveloped_hints(mut self) -> Vec<InlayHint> {
        envelope_host_inlay_hints(
            &mut self.hints,
            &self.server_name,
            &self.host_uri,
            InlayHintDocumentRevision {
                incarnation: self.incarnation,
                content_version: self.content_version,
            },
            self.connection_generation,
            &self.connection_key,
            self.server_resolves,
        );
        self.hints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> InlayHintEnvelope {
        serde_json::from_value(serde_json::json!({
            "origin": "test",
            "host_uri": "file:///test.md",
            "region_id": "region",
            "injection_language": "lua",
            "incarnation": 1,
            "content_version": 1,
            "connection_generation": 1,
            "offset": { "line": 3, "column": 2 },
            "inner": null
        }))
        .unwrap()
    }

    fn render(result: FanInResult<Option<Vec<u8>>>) -> String {
        format!("{result:?}")
    }

    /// One region's result passes through as it is, so a range inside a
    /// single region answers exactly as it did before regions were merged.
    #[test]
    fn merge_region_results_passes_a_single_region_through() {
        for single in [
            FanInResult::Done(Some(vec![1])),
            FanInResult::Done(Some(vec![])),
            FanInResult::Done(None),
            FanInResult::NoResult { errors: 0 },
            FanInResult::NoResult { errors: 2 },
            FanInResult::Cancelled,
        ] {
            let expected = render(match &single {
                FanInResult::Done(v) => FanInResult::Done(v.clone()),
                FanInResult::NoResult { errors } => FanInResult::NoResult { errors: *errors },
                FanInResult::Cancelled => FanInResult::Cancelled,
            });
            assert_eq!(
                render(merge_region_results(vec![single], |item| *item)),
                expected
            );
        }
    }

    #[test]
    fn merge_region_results_appends_answers_in_document_order() {
        let merged = merge_region_results(
            vec![
                FanInResult::Done(Some(vec![1, 2])),
                FanInResult::NoResult { errors: 1 },
                FanInResult::Done(None),
                FanInResult::Done(Some(vec![3])),
            ],
            |item| *item,
        );
        assert_eq!(
            render(merged),
            render(FanInResult::Done(Some(vec![1, 2, 3])))
        );
    }

    /// Overlapping same-depth regions (#350) interleave their hints, so
    /// appending them region by region would break document order: the
    /// merge sorts by position, stably, so hints at one position keep the
    /// order they arrived in (region order, then each response's order).
    #[test]
    fn merge_region_results_orders_interleaved_regions_by_position() {
        let merged = merge_region_results(
            vec![
                FanInResult::Done(Some(vec![(3, 'a'), (9, 'a')])),
                FanInResult::Done(Some(vec![(4, 'b'), (3, 'b'), (8, 'b')])),
            ],
            |&(line, _)| line,
        );
        let FanInResult::Done(Some(items)) = merged else {
            panic!("expected merged hints");
        };
        assert_eq!(items, [(3, 'a'), (3, 'b'), (4, 'b'), (8, 'b'), (9, 'a')]);
    }

    /// A single region's hints keep the server's own order, even unsorted,
    /// as they did before regions were merged.
    #[test]
    fn merge_region_results_leaves_a_single_regions_order_alone() {
        let merged = merge_region_results(
            vec![
                FanInResult::Done(Some(vec![9, 3])),
                FanInResult::Done(None),
                FanInResult::NoResult { errors: 0 },
            ],
            |item| *item,
        );
        assert_eq!(render(merged), render(FanInResult::Done(Some(vec![9, 3]))));
    }

    /// With no region answering, the request reports no result once, with
    /// every region's errors, so its log level reflects any failure.
    #[test]
    fn merge_region_results_reports_no_result_once_with_all_errors() {
        let merged = merge_region_results(
            vec![
                FanInResult::<Option<Vec<u8>>>::NoResult { errors: 1 },
                FanInResult::Done(None),
                FanInResult::NoResult { errors: 2 },
            ],
            |item| *item,
        );
        assert_eq!(render(merged), render(FanInResult::NoResult { errors: 3 }));
        let merged = merge_region_results(
            vec![
                FanInResult::<Option<Vec<u8>>>::Done(None),
                FanInResult::Done(None),
            ],
            |item| *item,
        );
        assert_eq!(render(merged), render(FanInResult::Done(None)));
    }

    fn range(start: (u32, u32), end: (u32, u32)) -> Range {
        Range {
            start: tower_lsp_server::ls_types::Position::new(start.0, start.1),
            end: tower_lsp_server::ls_types::Position::new(end.0, end.1),
        }
    }

    /// A region at host line 1 whose lines carry a two-column prefix.
    fn prefixed() -> crate::lsp::bridge::RegionOffset {
        crate::lsp::bridge::RegionOffset::with_per_line_offsets(1, vec![2, 2, 2])
    }

    #[test]
    fn clip_to_line_text_drops_a_range_inside_a_prefix() {
        assert_eq!(clip_to_line_text(range((2, 0), (2, 1)), &prefixed()), None);
        assert_eq!(clip_to_line_text(range((2, 0), (2, 2)), &prefixed()), None);
        assert_eq!(clip_to_line_text(range((2, 1), (2, 1)), &prefixed()), None);
    }

    #[test]
    fn clip_to_line_text_moves_prefix_endpoints_to_the_text() {
        assert_eq!(
            clip_to_line_text(range((2, 1), (2, 5)), &prefixed()),
            Some(range((2, 2), (2, 5)))
        );
        assert_eq!(
            clip_to_line_text(range((1, 4), (3, 0)), &prefixed()),
            Some(range((1, 4), (3, 2)))
        );
    }

    /// Ranges already on embedded text, including a caret at a line's text
    /// start and any range in a region without prefixes, are kept as they
    /// are.
    #[test]
    fn clip_to_line_text_keeps_ranges_on_text() {
        for kept in [
            range((2, 2), (2, 2)),
            range((1, 2), (3, 7)),
            range((2, 3), (2, 4)),
        ] {
            assert_eq!(clip_to_line_text(kept, &prefixed()), Some(kept));
        }
        let unprefixed = crate::lsp::bridge::RegionOffset::with_per_line_offsets(3, vec![0, 0]);
        for kept in [range((3, 0), (4, 0)), range((4, 0), (4, 0))] {
            assert_eq!(clip_to_line_text(kept, &unprefixed), Some(kept));
        }
    }

    #[test]
    fn inlay_hint_resolve_rejects_non_contiguous_region() {
        let envelope = envelope();
        let offset = crate::lsp::bridge::RegionOffset::new(3, 2);

        assert!(inlay_hint_region_is_resolvable(
            &envelope, &offset, true, "lua"
        ));
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &offset, false, "lua"
        ));
    }

    /// The envelope's language is client-echoed and names the virtual
    /// document the request is sent to; one that no longer matches the live
    /// region, or was emptied, must fail soft before a URI is built from it.
    #[test]
    fn inlay_hint_resolve_rejects_a_region_whose_language_differs() {
        let mut envelope = envelope();
        let offset = crate::lsp::bridge::RegionOffset::new(3, 2);

        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &offset, true, "python"
        ));
        envelope.injection_language.clear();
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &offset, true, "lua"
        ));
    }

    /// A region that moved (a different start line or first-line column) or
    /// whose per-line column table changed is a different geometry from the
    /// one the envelope was minted under; contiguity alone must not admit it.
    #[test]
    fn inlay_hint_resolve_rejects_a_region_whose_offset_moved() {
        let envelope = envelope();

        let moved = crate::lsp::bridge::RegionOffset::new(4, 2);
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &moved, true, "lua"
        ));
        let shifted_column = crate::lsp::bridge::RegionOffset::new(3, 3);
        assert!(!inlay_hint_region_is_resolvable(
            &envelope,
            &shifted_column,
            true,
            "lua"
        ));
        let per_line = crate::lsp::bridge::RegionOffset::with_per_line_offsets(3, vec![2, 1]);
        assert!(!inlay_hint_region_is_resolvable(
            &envelope, &per_line, true, "lua"
        ));
    }
}
