//! `textDocument/rangeFormatting` handler.
//!
//! Range formatting is the partial-document counterpart of full formatting:
//! it carries a `Range` and is expected to return edits that only modify
//! that range. Implementation reuses the full-formatting fan-out shape (one
//! `dispatch_preferred` per injection region) but pre-filters regions to
//! those that overlap the requested range and **clips the request range to
//! each region's bounds** before dispatching. Without that clip, an
//! injection that the request only partially covers would be asked to
//! format outside the user's selection.
//!
//! Edits returned for non-overlapping regions are skipped entirely.

use std::sync::Arc;

use tokio::task::JoinSet;
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{DocumentRangeFormattingParams, Position, Range, TextEdit};

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

        // Byte-level overlap pre-check uses the actual host text. The
        // line-only `clip_request_range_to_region` below is correct for
        // multi-line injections (code fences etc., `start_column == 0`),
        // but for inline injections (`start_column > 0`, e.g. backtick
        // code inside a markdown paragraph) it would treat the same line
        // as overlapping even when the request is entirely before the
        // injected content. Using the region's `byte_range` against the
        // request's byte span filters those out before we ask a
        // downstream formatter for a zero-width / before-injection range.
        let mapper = PositionMapper::new(snapshot.text());
        let request_bytes = mapper
            .position_to_byte(host_range.start)
            .zip(mapper.position_to_byte(host_range.end))
            .map(|(start, end)| start..end);

        let mut outer_join_set: JoinSet<Option<Vec<TextEdit>>> = JoinSet::new();

        for resolved in all_regions {
            // Skip regions whose host byte range does not actually overlap
            // the request — protects against inline-injection
            // false-positives that the line-only clip below would miss.
            if let Some(ref req_bytes) = request_bytes
                && (req_bytes.end <= resolved.region.byte_range.start
                    || req_bytes.start >= resolved.region.byte_range.end)
            {
                continue;
            }

            // Clip the editor-supplied range to this region's host line
            // bounds. After the byte-level overlap check above, this is
            // safe to do in line space — any column saturation inside
            // `translate_host_range_to_virtual` only fires on partial
            // overlaps that already passed the byte check.
            let Some(clipped_host_range) = clip_request_range_to_region(
                host_range,
                resolved.region.line_range.start,
                resolved.region.line_range.end,
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

/// Clip an editor-supplied `Range` to a region's host-line bounds.
///
/// `region_line_lo..region_line_hi_exclusive` describes the region's host
/// line span (matching `CacheableInjectionRegion::line_range`, which is
/// half-open). Returns `None` when the request range does not overlap the
/// region at all — the handler skips such regions outright.
///
/// Overlap is computed in line space using half-open intervals:
///
/// - Region span: `[region_line_lo, region_line_hi_exclusive)`
/// - Request span: `[req.start.line, req.end.line)` when `req.end.character == 0`;
///   otherwise `[req.start.line, req.end.line + 1)` so a selection ending
///   mid-line still counts that line as covered.
///
/// The returned range preserves the request's endpoints whenever they lie
/// inside the region; endpoints that fall outside snap to the region's
/// boundary at column 0 (start of the boundary line).
fn clip_request_range_to_region(
    request: Range,
    region_line_lo: u32,
    region_line_hi_exclusive: u32,
) -> Option<Range> {
    let req_lo = request.start.line;
    // Per LSP, `Range.end` is exclusive. When `end.character == 0`, the end
    // sits at the start of a line — that line is NOT included in the
    // selection. For all other end characters, the end line IS covered.
    let req_hi_exclusive = if request.end.character == 0 {
        request.end.line
    } else {
        request.end.line.saturating_add(1)
    };

    let clipped_lo = req_lo.max(region_line_lo);
    let clipped_hi_exclusive = req_hi_exclusive.min(region_line_hi_exclusive);

    if clipped_lo >= clipped_hi_exclusive {
        return None;
    }

    let clipped_start = if clipped_lo == req_lo {
        request.start
    } else {
        Position {
            line: clipped_lo,
            character: 0,
        }
    };
    let clipped_end = if clipped_hi_exclusive == req_hi_exclusive {
        request.end
    } else {
        Position {
            line: clipped_hi_exclusive,
            character: 0,
        }
    };

    Some(Range {
        start: clipped_start,
        end: clipped_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;

    fn range(start_line: u32, start_char: u32, end_line: u32, end_char: u32) -> Range {
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

    #[test]
    fn clip_returns_request_unchanged_when_fully_inside_region() {
        let clipped = clip_request_range_to_region(range(5, 2, 9, 4), 3, 12).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 5,
                character: 2
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 9,
                character: 4
            }
        );
    }

    #[test]
    fn clip_snaps_start_when_request_begins_before_region() {
        // Request: lines 0..10; region: lines 4..15. Start clipped to (4, 0).
        let clipped = clip_request_range_to_region(range(0, 0, 10, 0), 4, 15).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 4,
                character: 0
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 10,
                character: 0
            }
        );
    }

    #[test]
    fn clip_snaps_end_when_request_extends_past_region() {
        // Request: lines 2..20; region: lines 0..12. End clipped to (12, 0).
        let clipped = clip_request_range_to_region(range(2, 0, 20, 5), 0, 12).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 2,
                character: 0
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 12,
                character: 0
            }
        );
    }

    #[test]
    fn clip_returns_none_when_request_is_entirely_before_region() {
        let clipped = clip_request_range_to_region(range(0, 0, 3, 0), 5, 10);
        assert!(clipped.is_none(), "no overlap → no work");
    }

    #[test]
    fn clip_returns_none_when_request_is_entirely_after_region() {
        let clipped = clip_request_range_to_region(range(20, 0, 25, 0), 5, 10);
        assert!(clipped.is_none(), "no overlap → no work");
    }

    #[test]
    fn clip_returns_none_when_request_touches_region_boundary_at_start() {
        // Request: lines 0..5 with end.character == 0 → covers [0, 5),
        // region: [5, 10). No overlap.
        let clipped = clip_request_range_to_region(range(0, 0, 5, 0), 5, 10);
        assert!(clipped.is_none());
    }

    #[test]
    fn clip_covers_single_line_with_mid_line_end_character() {
        // Request: line 3, characters 0..5 → covers line 3.
        // Region: lines 3..4. Overlap covers exactly line 3.
        let clipped = clip_request_range_to_region(range(3, 0, 3, 5), 3, 4).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 3,
                character: 0
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 3,
                character: 5
            }
        );
    }

    #[test]
    fn clip_snaps_both_endpoints_when_request_spans_region() {
        // Request: lines 0..100; region: lines 10..20. Result: (10,0)..(20,0).
        let clipped = clip_request_range_to_region(range(0, 0, 100, 0), 10, 20).unwrap();
        assert_eq!(
            clipped.start,
            Position {
                line: 10,
                character: 0
            }
        );
        assert_eq!(
            clipped.end,
            Position {
                line: 20,
                character: 0
            }
        );
    }
}
