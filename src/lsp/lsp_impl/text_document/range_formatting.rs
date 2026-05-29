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

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentRangeFormattingParams, Range, TextEdit};

use crate::language::InjectionResolver;
use crate::lsp::aggregation::server::FanInResult;
use crate::lsp::aggregation::server::dispatch_preferred;
use crate::lsp::lsp_impl::bridge_context::DocumentRequestContext;
use crate::text::PositionMapper;

use super::super::{Kakehashi, uri_to_url};
use super::formatting::finalize_formatting_edits;

impl Kakehashi {
    pub(crate) async fn range_formatting_impl(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
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

        let Some(injection_query) = self.language.injection_query(&language_name) else {
            return Ok(None);
        };

        let all_regions = InjectionResolver::resolve_all(
            &self.language,
            self.bridge.region_id_tracker(),
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

        for resolved in all_regions {
            let Some(clipped_host_range) =
                clip_request_to_region(&request_bytes, &resolved.region.byte_range, &mapper)
            else {
                continue;
            };

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
                "textDocument/rangeFormatting",
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

            outer_join_set.spawn(async move {
                let result = dispatch_preferred(
                    &region_ctx,
                    pool.clone(),
                    move |t| {
                        let options = options.clone();
                        async move {
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
                                )
                                .await
                        }
                    },
                    // Same semantics as full formatting: `Some(vec![])` is
                    // an authoritative "no edits needed" (e.g., the range is
                    // already perfectly formatted) — keep it and stop. `None`
                    // means "no response" and falls through to lower-priority
                    // servers.
                    |opt| opt.is_some(),
                    region_cancel_rx,
                )
                .await;
                match result {
                    FanInResult::Done(edits) => edits,
                    FanInResult::NoResult { .. } | FanInResult::Cancelled => None,
                }
            });
        }

        let response = finalize_formatting_edits(outer_join_set, cancel_state.token.clone()).await;
        pool.unregister_all_for_upstream_id(upstream_request_id.as_ref());
        response
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
/// `start.min(end)` guards against an inverted result: a malformed client may
/// send `start` after `end`, and clamping the start's line could otherwise
/// push it past an in-bounds end. A collapsed (empty) range is skipped
/// harmlessly by `clip_request_to_region`.
fn clamp_request_to_document(mapper: &PositionMapper, range: Range) -> std::ops::Range<usize> {
    let start = mapper.position_to_byte_clamped(range.start);
    let end = mapper.position_to_byte_clamped(range.end);
    start.min(end)..end
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
    fn request_bytes_never_inverts_when_start_is_after_end() {
        // Malformed client: start positioned after end. The result must not
        // invert (start <= end) so downstream clipping behaves sanely.
        let text = "first line\nsecond line\n";
        let mapper = PositionMapper::new(text);
        let range = pos_range(1, 5, 0, 2);

        let bytes = clamp_request_to_document(&mapper, range);
        assert!(bytes.start <= bytes.end, "byte range must not invert");
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
