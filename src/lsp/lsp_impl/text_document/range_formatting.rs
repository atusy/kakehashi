//! `textDocument/rangeFormatting` handler.
//!
//! Range formatting is the partial-document counterpart of full formatting:
//! it carries a `Range` and is expected to return edits that only modify
//! that range. Implementation reuses the full-formatting fan-out shape (one
//! `dispatch_preferred` per injection region) but clips the request to
//! each region's `byte_range` before dispatching — so an inline injection
//! (`start_column > 0`) that the request only partially covers is never
//! asked to format positions outside the injected content.
//!
//! Regions whose byte range is disjoint from the request are skipped
//! entirely; no downstream request is sent.
//!
//! When the request fully covers a region (its byte span encloses the whole
//! `byte_range`), the handler prefers a full `textDocument/formatting` request
//! for that region: the two are equivalent when the entire injected document
//! is selected, and full formatting is also honored by downstream servers that
//! implement `formatting` but not `rangeFormatting`. If such a covering
//! request finds the server has no `documentFormattingProvider` (full
//! formatting returns no result), it falls back to `rangeFormatting` so
//! range-only servers still format.

use std::sync::{Arc, Mutex};

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentRangeFormattingParams, NumberOrString, Range, TextEdit};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::{
    dispatch_preferred, dispatch_preferred_with_tokens, mint_region_progress_source,
};
use crate::lsp::bridge::{ClientProgressAggregator, ClientProgressDeregisterGuard};
use crate::lsp::lsp_impl::bridge_context::{DocumentRequestContext, parse_host_verbatim};
use crate::text::PositionMapper;

use super::super::{Kakehashi, uri_to_url};
use super::formatting::{finalize_formatting_edits, unique_edit_regions};

impl Kakehashi {
    pub(crate) async fn range_formatting_impl(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let raw_params = serde_json::to_value(&params).unwrap_or(serde_json::Value::Null);
        let lsp_uri = params.text_document.uri.clone();
        let work_done_token = params.work_done_progress_params.work_done_token.clone();

        // Explicit-action bounded wait (parse-snapshot ADR §3), the same
        // treatment — and the same placement, BEFORE the layer race — as its
        // sibling `formatting` (the two share the "textDocument/formatting"
        // configuration key and the spec's explicit-action class):
        // user-triggered and infrequent, so it may briefly wait for the
        // in-flight parse; a still-stale snapshot after the wait rejects with
        // ContentModified — the request's range is authored against the LIVE
        // text, so silently no-opping (or worse, formatting a trailing
        // region) is the jarring outcome §3 forbids. Hoisted above the layer
        // closures so it applies under every layer-order configuration.
        // Never-parsed/gone falls through to the layers' empty fallbacks.
        if let Ok(wait_uri) = uri_to_url(&lsp_uri)
            && let crate::lsp::lsp_impl::snapshot_read::SnapshotWait::Stale = self
                .wait_for_current_snapshot(&wait_uri, std::time::Duration::from_millis(500))
                .await
        {
            return Err(crate::error::content_modified_error());
        }

        let virt = async {
            let lsp_uri = params.text_document.uri;
            let options = params.options;
            let host_range = params.range;

            let Ok(uri) = uri_to_url(&lsp_uri) else {
                log::warn!("Invalid URI in rangeFormatting: {}", lsp_uri.as_str());
                return Ok(None);
            };

            log::debug!("rangeFormatting called for {} range {:?}", uri, host_range);

            let snapshot = match self.documents.get(&uri) {
                None => {
                    log::debug!("rangeFormatting: No document found for {}", uri);
                    return Ok(None);
                }
                Some(doc) => match doc.snapshot() {
                    None => {
                        log::debug!(
                            "rangeFormatting: Document not fully initialized for {}",
                            uri
                        );
                        return Ok(None);
                    }
                    Some(snapshot) => snapshot,
                },
            };

            let Some(language_name) = self.document_language(&uri) else {
                log::debug!(target: "kakehashi::rangeFormatting", "No language detected");
                return Ok(None);
            };

            // Layer gating keys off "textDocument/formatting", matching the
            // aggregation config below: range formatting is the partial-document
            // counterpart of full formatting and shares its configuration key.
            if !self.virt_layer_enabled(&language_name, "textDocument/formatting") {
                log::debug!(
                    target: "kakehashi::rangeFormatting",
                    "virt layer disabled for {} via layers.aggregation priorities",
                    language_name
                );
                return Ok(None);
            }

            let Some(injection_query) = self.language.injection_query(&language_name) else {
                return Ok(None);
            };

            let all_regions = match self
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
            };

            if all_regions.is_empty() {
                return Ok(None);
            }

            let upstream_request_id = crate::lsp::current_upstream_id();
            let cancel_state = self.setup_formatting_cancel_token(upstream_request_id.as_ref());
            let pool = self.bridge.pool_arc();

            // Clip the request to each region's actual byte bounds (not just
            // its line span). Line-only clipping is incorrect for two cases
            // that matter for inline injections (`start_column > 0`):
            //
            // 1. Request shares a line with the injection but lies entirely
            //    before/after the injected content. Byte intersection
            //    correctly skips the region; line-only would mis-overlap.
            // 2. Request straddles an injection boundary (e.g., starts before
            //    the injection's opening column, ends past its closing
            //    column on the same line). Byte intersection clips the
            //    endpoints to the injected content; line-only would pass the
            //    out-of-bounds columns to `translate_host_range_to_virtual`,
            //    where `saturating_sub` produces a virtual range past the
            //    virtual document's actual columns and the downstream
            //    formatter may error or format more than the user selected.
            //
            // Multi-line code-fence injections (`start_column == 0`) are
            // unchanged because their line and byte bounds are equivalent.
            let mapper = PositionMapper::new(snapshot.text());
            let request_bytes = clamp_request_to_document(&mapper, host_range);

            let mut outer_join_set: JoinSet<Option<Vec<TextEdit>>> = JoinSet::new();

            // Shared client progress: a range-formatting request fans out over
            // several injection regions (one `dispatch_preferred` each), so they
            // share ONE aggregator + ONE teardown guard. The aggregator's winner
            // rule shows the first region to begin as one coherent
            // `Begin → … → End` on the editor's token (ls-bridge-client-progress).
            // `None` when no `workDoneToken`.
            let shared_cp = work_done_token.map(|client_token| {
                (
                    Arc::new(Mutex::new(ClientProgressAggregator::new(client_token))),
                    Arc::clone(pool.client_progress_registry()),
                )
            });
            let mut cp_minted: Vec<NumberOrString> = Vec::new();

            for resolved in unique_edit_regions(&all_regions) {
                // Non-contiguous combined injections contain masked host-only
                // gaps. Even a covering range may fall back to full formatting,
                // whose single replacement would overwrite those real gaps.
                if !resolved.contiguous {
                    continue;
                }
                // A covering request (its byte span encloses the whole region)
                // prefers full formatting; a partial request range-formats the
                // clipped span. Either way we compute the clipped+content-clamped
                // host range: the partial path sends it, and the covering path
                // keeps it as a fallback for servers that support `rangeFormatting`
                // but not `formatting`. For a covering request, clipping against
                // the region yields the whole region.
                let is_covering =
                    request_covers_region(&request_bytes, &resolved.region.byte_range);

                let Some(clipped) =
                    clip_request_to_region(&request_bytes, &resolved.region.byte_range, &mapper)
                else {
                    continue;
                };
                // The byte-range clip can still leave an endpoint inside a stripped
                // per-line prefix (e.g. blockquoted code's `> `); pull both
                // endpoints onto the actual injected content. A partial selection
                // lying entirely in prefix bytes collapses and the region is
                // skipped (a covering selection spans real content, so it never
                // collapses).
                let Some(clipped_host_range) = clamp_range_to_content_columns(
                    clipped,
                    resolved.region.line_range.start,
                    &resolved.line_column_offsets,
                ) else {
                    continue;
                };

                let configs = self.bridge_configs_for_injection_language(
                    &language_name,
                    &resolved.injection_language,
                );
                if configs.is_empty() {
                    continue;
                }

                // Resolve aggregation under "textDocument/formatting", not
                // "textDocument/rangeFormatting". Range formatting is the
                // partial-document counterpart of full formatting and shares its
                // server priorities and strategy (see language-server-bridge-request-strategies, which groups the
                // two). There is no separate rangeFormatting config key, and
                // `resolve_aggregation_entry` only falls back method → `_`
                // wildcard — never formatting → rangeFormatting — so keying off
                // "textDocument/rangeFormatting" would silently ignore a user's
                // "textDocument/formatting" configuration.
                let agg = self.resolve_aggregation_config(
                    &language_name,
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

                // Mint this region's tracked-source token into the shared
                // aggregator (the per-region map dispatch hands to its winning
                // downstream).
                let region_cp_tokens = shared_cp.as_ref().and_then(|(aggregator, registry)| {
                    mint_region_progress_source(&region_ctx, registry, aggregator)
                });
                if let Some(map) = &region_cp_tokens {
                    cp_minted.extend(map.values().cloned());
                }

                let pool = Arc::clone(&pool);
                let options = options.clone();
                let region_cancel_rx = cancel_state.derive_receiver();

                outer_join_set.spawn(async move {
                    let send = move |t: crate::lsp::aggregation::server::FanOutTask| {
                        let options = options.clone();
                        async move {
                            // The covering path may issue BOTH a formatting and
                            // a rangeFormatting request; capture the per-task
                            // client-progress token once and hand it to whichever
                            // call actually goes on the wire.
                            let cp_token = t.client_progress_token;
                            // Covering request: prefer full formatting, but fall
                            // back to rangeFormatting over the whole region when
                            // the server has no `documentFormattingProvider`
                            // (`Ok(None)`) — otherwise a range-only server would
                            // format nothing. Errors propagate (no fallback);
                            // `Ok(Some(_))` (incl. an empty edit list) is an
                            // authoritative result.
                            if is_covering {
                                match t
                                    .pool
                                    .send_formatting_request(
                                        &t.server_name,
                                        &t.server_config,
                                        &t.uri,
                                        &t.injection_language,
                                        &t.region_id,
                                        t.offset.clone(),
                                        &t.virtual_content,
                                        options.clone(),
                                        t.upstream_id.clone(),
                                        cp_token.clone(),
                                        None,
                                    )
                                    .await
                                {
                                    Ok(None) => {} // fall through to rangeFormatting
                                    other => return other,
                                }
                            }
                            t.pool
                                .send_range_formatting_request(
                                    &t.server_name,
                                    &t.server_config,
                                    &t.uri,
                                    &t.injection_language,
                                    &t.region_id,
                                    t.offset,
                                    &t.virtual_content,
                                    clipped_host_range,
                                    options,
                                    t.upstream_id,
                                    cp_token,
                                    None,
                                )
                                .await
                        }
                    };
                    // Same semantics as full formatting: `Some(vec![])` is an
                    // authoritative "no edits needed" (e.g., the range is already
                    // perfectly formatted) — keep it and stop. `None` means "no
                    // response" and falls through to lower-priority servers.
                    let is_nonempty = |opt: &Option<Vec<TextEdit>>| opt.is_some();
                    let result = match region_cp_tokens {
                        Some(tokens) => {
                            dispatch_preferred_with_tokens(
                                &region_ctx,
                                pool.clone(),
                                send,
                                is_nonempty,
                                region_cancel_rx,
                                tokens,
                            )
                            .await
                        }
                        None => {
                            dispatch_preferred(
                                &region_ctx,
                                pool.clone(),
                                send,
                                is_nonempty,
                                region_cancel_rx,
                            )
                            .await
                        }
                    };
                    match result {
                        FanInResult::Done(edits) => edits,
                        FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                    }
                });
            }

            // One teardown guard for the whole request, held across the edit
            // collection so the synthetic terminal `End` fires once, after every
            // region settles (or on cancel). Dropped at the end of this block.
            let _cp_guard = shared_cp.map(|(aggregator, registry)| {
                ClientProgressDeregisterGuard::new(
                    registry,
                    cp_minted,
                    aggregator,
                    pool.upstream_tx(),
                )
            });

            finalize_formatting_edits(outer_join_set, cancel_state.token.clone()).await
        };

        // layer_method keys off "textDocument/formatting" ON PURPOSE: range
        // formatting is the partial-document counterpart of full formatting
        // and shares its layer order and aggregation key. request_method is
        // what goes on the wire to host-capable servers.
        self.walk_layers(
            &lsp_uri,
            "textDocument/formatting",
            "textDocument/rangeFormatting",
            raw_params,
            virt,
            parse_host_verbatim::<Vec<TextEdit>>,
            |edits: &Vec<TextEdit>| !edits.is_empty(),
        )
        .await
    }
}

/// Map a host request `Range` to a byte range, clamping out-of-bounds
/// endpoints to the nearest valid offset instead of dropping the request.
///
/// Editors routinely send positions past the document or line bounds: a range
/// that runs "to the end of the file" (often `u32::MAX`), or a character index
/// past a line's end. `position_to_byte` returns `None` for both, which would
/// silently skip formatting of otherwise-valid regions. `position_to_byte_clamped`
/// resolves each endpoint precisely — a character past a line's end clamps to
/// that line's end (within the line; not the document end, which would balloon
/// a single-line request into later injection regions); a line past EOF clamps
/// to the document end.
///
/// A malformed client may send `start` after `end` (or clamping the start's
/// line could push it past an in-bounds end). We normalize such an inverted
/// pair to `[min, max]` rather than collapsing it to an empty range — the two
/// positions still describe the span the user selected, so formatting it is
/// more useful than silently doing nothing.
fn clamp_request_to_document(mapper: &PositionMapper, range: Range) -> std::ops::Range<usize> {
    let start = mapper.position_to_byte_clamped(range.start);
    let end = mapper.position_to_byte_clamped(range.end);
    start.min(end)..start.max(end)
}

/// Intersect a request's byte range with a region's byte range and map the
/// result back to host LSP positions.
///
/// Returns `None` when the intervals are disjoint — the handler skips such
/// regions. Returns the clipped host `Range` otherwise, with both endpoints
/// converted from byte offsets back to LSP `Position`s via `mapper`.
///
/// Byte-precision clipping is what makes `range_formatting_impl` safe for
/// inline injections (`start_column > 0`). A line-only clip would
/// (1) treat a same-line request that lies entirely before/after the
/// injected content as overlapping, and (2) pass host columns outside the
/// injected content to `translate_host_range_to_virtual`, where
/// `saturating_sub` would produce a virtual range past the virtual
/// document's actual columns.
fn clip_request_to_region(
    request_bytes: &std::ops::Range<usize>,
    region_bytes: &std::ops::Range<usize>,
    mapper: &PositionMapper,
) -> Option<Range> {
    let clipped_start = request_bytes.start.max(region_bytes.start);
    let clipped_end = request_bytes.end.min(region_bytes.end);
    if clipped_start >= clipped_end {
        return None;
    }
    let start = mapper.byte_to_position(clipped_start)?;
    let end = mapper.byte_to_position(clipped_end)?;
    Some(Range { start, end })
}

/// Whether the request fully covers the injection region — i.e. the request's
/// byte span encloses the region's entire `byte_range`.
///
/// When this holds, the user selected the whole injected document, so a full
/// `textDocument/formatting` request is equivalent to range-formatting the
/// clipped span — and is honored by downstream servers that implement
/// `formatting` but not `rangeFormatting`. The handler dispatches full
/// formatting in that case and range formatting otherwise.
fn request_covers_region(
    request_bytes: &std::ops::Range<usize>,
    region_bytes: &std::ops::Range<usize>,
) -> bool {
    request_bytes.start <= region_bytes.start && request_bytes.end >= region_bytes.end
}

/// Clamp a clipped host range so neither endpoint sits inside an injection's
/// stripped per-line prefix (e.g. the `> ` of a blockquoted code block).
///
/// `clip_request_to_region` intersects against the injection's *raw*
/// `byte_range`, but that range still contains the per-line prefix bytes that
/// `extract_clean_content` strips out of `virtual_content`. An endpoint left
/// inside such a prefix would be carried into `translate_host_range_to_virtual`,
/// whose `saturating_sub` collapses it to virtual column 0 — so the clip alone
/// is not aligned with the bytes that actually became the virtual document.
///
/// `line_column_offsets[v]` is the host column where content begins on virtual
/// line `v` (the same per-line offset the bridge subtracts during translation).
/// Clamping each endpoint up to its line's content-start column keeps the
/// request inside the injected content, and lets a selection lying entirely in
/// prefix bytes collapse to an empty range that the caller skips instead of
/// dispatching a no-op downstream request.
///
/// No-op for the common cases: code-fence injections have all-zero offsets, and
/// inline injections already start at content (their prefix bytes lie outside
/// `byte_range`), so neither is altered.
fn clamp_range_to_content_columns(
    mut range: Range,
    base_line: u32,
    line_column_offsets: &[u32],
) -> Option<Range> {
    let content_col = |host_line: u32| -> u32 {
        let virtual_line = host_line.saturating_sub(base_line) as usize;
        line_column_offsets.get(virtual_line).copied().unwrap_or(0)
    };
    range.start.character = range.start.character.max(content_col(range.start.line));
    range.end.character = range.end.character.max(content_col(range.end.line));

    // Empty or inverted after clamping means nothing inside the content is
    // selected on this region — skip it rather than dispatch an empty request.
    if (range.start.line, range.start.character) >= (range.end.line, range.end.character) {
        return None;
    }
    Some(range)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;

    /// Build a `Range` from line/character endpoints. Tests use this for
    /// readability against expected output.
    fn pos_range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Range {
        Range {
            start: Position {
                line: start_line,
                character: start_char,
            },
            end: Position {
                line: end_line,
                character: end_char,
            },
        }
    }

    /// Build a request byte range from positions using the same mapper the
    /// handler would use, so tests stay in sync with the helper's contract.
    fn bytes(mapper: &PositionMapper, range: Range) -> std::ops::Range<usize> {
        mapper.position_to_byte(range.start).unwrap()..mapper.position_to_byte(range.end).unwrap()
    }

    #[test]
    fn covers_region_true_when_request_encloses_region() {
        assert!(request_covers_region(&(0..100), &(10..20)));
    }

    #[test]
    fn covers_region_true_when_request_equals_region() {
        assert!(request_covers_region(&(10..20), &(10..20)));
    }

    #[test]
    fn covers_region_false_when_request_starts_after_region_start() {
        assert!(!request_covers_region(&(12..20), &(10..20)));
    }

    #[test]
    fn covers_region_false_when_request_ends_before_region_end() {
        assert!(!request_covers_region(&(10..18), &(10..20)));
    }

    #[test]
    fn covers_region_false_for_partial_overlap() {
        assert!(!request_covers_region(&(0..15), &(10..20)));
    }

    #[test]
    fn request_bytes_clamps_out_of_bounds_end_to_document_length() {
        // Editors send a range that runs past EOF when formatting "to the
        // end of the file" (often `u32::MAX`). The end must clamp to the
        // document length instead of dropping the whole request.
        let text = "first line\nsecond line\n";
        let mapper = PositionMapper::new(text);
        let range = pos_range(0, 0, 999, 0);

        let bytes = clamp_request_to_document(&mapper, range);
        assert_eq!(bytes, 0..text.len());
    }

    #[test]
    fn request_bytes_clamps_out_of_bounds_start_to_document_length() {
        // A start past EOF clamps to the document end, yielding an empty
        // (degenerate) range that downstream clipping skips harmlessly.
        let text = "first line\nsecond line\n";
        let mapper = PositionMapper::new(text);
        let range = pos_range(999, 0, 999, 5);

        let bytes = clamp_request_to_document(&mapper, range);
        assert_eq!(bytes, text.len()..text.len());
    }

    #[test]
    fn request_bytes_clamps_overlong_character_to_line_end_not_document_end() {
        // A character past a valid line's end must clamp to that line's end,
        // not the document end — otherwise a single-line request would
        // broaden into later lines and dispatch to regions the user did not
        // select. Line 0 is "first line\n" (bytes 0..11), so col 999 clamps
        // to byte 11 (start of line 1), well short of the document end (23).
        let text = "first line\nsecond line\n";
        let mapper = PositionMapper::new(text);
        let range = pos_range(0, 0, 0, 999);

        let bytes = clamp_request_to_document(&mapper, range);
        assert_eq!(bytes, 0..11);
    }

    #[test]
    fn request_bytes_normalizes_inverted_range_to_forward_span() {
        // Malformed client: start positioned after end. The pair is
        // normalized to [min, max] (the span the user selected) rather than
        // collapsing to an empty range, so formatting still happens.
        // "first line\n" is 11 bytes; (0,2) -> byte 2, (1,5) -> byte 16.
        let text = "first line\nsecond line\n";
        let mapper = PositionMapper::new(text);
        let range = pos_range(1, 5, 0, 2);

        let bytes = clamp_request_to_document(&mapper, range);
        assert_eq!(bytes, 2..16);
    }

    #[test]
    fn content_clamp_is_noop_for_zero_offsets() {
        // Code-fence injection: all per-line offsets are 0, so nothing moves.
        let r = pos_range(2, 0, 4, 3);
        let clamped = clamp_range_to_content_columns(r, 2, &[0, 0, 0]).unwrap();
        assert_eq!(clamped, r);
    }

    #[test]
    fn content_clamp_pulls_prefix_start_onto_content() {
        // Blockquoted code: content starts at host col 2 on each line.
        // Region begins at host line 1; a start at (1,0) sits in the `> `
        // prefix and must clamp to col 2; the in-content end is untouched.
        let r = pos_range(1, 0, 2, 5);
        let clamped = clamp_range_to_content_columns(r, 1, &[2, 2]).unwrap();
        assert_eq!(
            clamped.start,
            Position {
                line: 1,
                character: 2
            }
        );
        assert_eq!(
            clamped.end,
            Position {
                line: 2,
                character: 5
            }
        );
    }

    #[test]
    fn content_clamp_skips_prefix_only_selection() {
        // Whole selection lies within the `> ` prefix of one line → after
        // clamping both endpoints to col 2 the range is empty → skipped.
        let r = pos_range(1, 0, 1, 1);
        assert!(clamp_range_to_content_columns(r, 1, &[2, 2]).is_none());
    }

    #[test]
    fn clip_returns_request_unchanged_when_fully_inside_region() {
        // Three-line document; region spans the second line entirely.
        // Request is fully inside the region's content.
        let text = "first line\nsecond line\nthird line\n";
        let mapper = PositionMapper::new(text);
        let req = pos_range(1, 2, 1, 7);
        let region = bytes(&mapper, pos_range(1, 0, 2, 0));

        let clipped = clip_request_to_region(&bytes(&mapper, req), &region, &mapper).unwrap();
        assert_eq!(clipped, req);
    }

    #[test]
    fn clip_snaps_start_when_request_begins_before_region() {
        // Request starts on line 0; region starts at line 1 col 0.
        let text = "first line\nsecond line\nthird line\n";
        let mapper = PositionMapper::new(text);
        let req = pos_range(0, 0, 1, 7);
        let region = bytes(&mapper, pos_range(1, 0, 2, 0));

        let clipped = clip_request_to_region(&bytes(&mapper, req), &region, &mapper).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 1,
                character: 0
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 1,
                character: 7
            }
        );
    }

    #[test]
    fn clip_snaps_end_when_request_extends_past_region() {
        let text = "first line\nsecond line\nthird line\n";
        let mapper = PositionMapper::new(text);
        let req = pos_range(1, 2, 2, 5);
        let region = bytes(&mapper, pos_range(1, 0, 2, 0));

        let clipped = clip_request_to_region(&bytes(&mapper, req), &region, &mapper).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 1,
                character: 2
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 2,
                character: 0
            }
        );
    }

    #[test]
    fn clip_returns_none_when_request_is_entirely_before_region() {
        let text = "first\nsecond\nthird\n";
        let mapper = PositionMapper::new(text);
        let req = bytes(&mapper, pos_range(0, 0, 0, 3));
        let region = bytes(&mapper, pos_range(1, 0, 2, 0));

        assert!(clip_request_to_region(&req, &region, &mapper).is_none());
    }

    #[test]
    fn clip_returns_none_when_request_is_entirely_after_region() {
        let text = "first\nsecond\nthird\n";
        let mapper = PositionMapper::new(text);
        let req = bytes(&mapper, pos_range(2, 0, 2, 3));
        let region = bytes(&mapper, pos_range(0, 0, 1, 0));

        assert!(clip_request_to_region(&req, &region, &mapper).is_none());
    }

    #[test]
    fn clip_skips_inline_injection_when_request_is_before_injected_content() {
        // Inline injection on a single line: paragraph text with backtick
        // code starting mid-line (`start_column > 0`). A line-only clip
        // would falsely overlap; byte-precision correctly skips.
        //
        //   "say `lua_inline` here"
        //    ^cols 0..3 "say"
        //         ^col 4..5 "`"
        //          ^cols 5..15 "lua_inline"  ← injection content
        //                    ^col 15..16 "`"
        let text = "say `lua_inline` here\n";
        let mapper = PositionMapper::new(text);
        let req = bytes(&mapper, pos_range(0, 0, 0, 3)); // "say"
        let region = bytes(&mapper, pos_range(0, 5, 0, 15)); // "lua_inline"

        assert!(
            clip_request_to_region(&req, &region, &mapper).is_none(),
            "request before the inline injection must not be dispatched"
        );
    }

    #[test]
    fn clip_clamps_inline_injection_when_request_straddles_boundary() {
        // Inline injection same as above; request straddles both ends.
        let text = "say `lua_inline` here\n";
        let mapper = PositionMapper::new(text);
        let req = bytes(&mapper, pos_range(0, 3, 0, 18)); // " `lua_inline` h"
        let region = bytes(&mapper, pos_range(0, 5, 0, 15)); // "lua_inline"

        let clipped = clip_request_to_region(&req, &region, &mapper).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 0,
                character: 5
            },
            "start snaps to injection's start_column"
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 0,
                character: 15
            },
            "end snaps to injection's end column"
        );
    }
}
