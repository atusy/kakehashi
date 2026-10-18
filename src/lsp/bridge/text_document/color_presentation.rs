//! Color presentation requests for downstream servers, handling the bidirectional
//! host↔virtual coordinate transformation. Like inlay hints, the request carries a
//! range (where the color was found) and the response may include textEdits and
//! additionalTextEdits whose ranges must be translated back to host coordinates.
//! Presentations whose (implicit or explicit) textEdit is unsafe for the
//! injection region — escapes it, breaks per-line `> ` prefixes, or merges
//! content into the closing fence — are dropped; unsafe additionalTextEdits
//! are stripped.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    Color, ColorPresentation, ColorPresentationParams, Position, Range, TextDocumentIdentifier,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    EditMode, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    host_range_in_prepared_gap, response_has_jsonrpc_error, text_edit_safe_in_region,
    translate_host_range_to_virtual, translate_virtual_main_and_additional_edits_to_host,
    translate_virtual_text_edits_to_host, virtual_uri_to_lsp_uri,
};

impl LanguageServerPool {
    /// Send a color presentation request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing color-presentation-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_color_presentation_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_range: Range,
        region_end: Position,
        color: Color,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Vec<ColorPresentation>> {
        let handle = self
            .get_or_create_virtual_connection(
                server_name,
                server_config,
                host_uri,
                injection_language,
                region_id,
            )
            .await?;
        if !handle.has_capability("textDocument/colorPresentation") {
            return Ok(vec![]);
        }
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_color_presentation_request(
                    virtual_uri,
                    host_range,
                    color,
                    &offset,
                    request_id,
                )
            },
            |response, ctx| {
                color_presentation_response_to_host(response, ctx, region_end, host_range)
            },
        )
        .await
    }
}

/// [`transform_color_presentation_response_to_host`] for a response as the
/// executor hands it over: nothing when the request's map no longer
/// describes the region's current host text (see `BridgeResponseContext`).
fn color_presentation_response_to_host(
    response: serde_json::Value,
    ctx: &super::super::pool::BridgeResponseContext<'_>,
    region_end: Position,
    host_request_range: Range,
) -> Vec<ColorPresentation> {
    if !ctx.request_map_current {
        return Vec::new();
    }
    transform_color_presentation_response_to_host(
        response,
        ctx.offset,
        region_end,
        host_request_range,
    )
}

/// Build a JSON-RPC color presentation request, translating the color's range from
/// host to virtual coordinates. Line translation uses `saturating_sub` to avoid an
/// underflow panic when a concurrent edit invalidates region data.
fn build_color_presentation_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    color: Color,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<ColorPresentationParams> {
    // Translate range from host to virtual coordinates
    let mut virtual_range = host_range;
    translate_host_range_to_virtual(&mut virtual_range, offset);

    let params = ColorPresentationParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        color,
        range: virtual_range,
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    JsonRpcRequest::new(
        request_id.as_i64(),
        "textDocument/colorPresentation",
        params,
    )
}

/// Transform a color presentation response from virtual to host coordinates,
/// translating each item's `textEdit` and `additionalTextEdits` while leaving
/// the label unchanged. On a prepared document they go through its map (their
/// text re-indented), a label without a `textEdit` becomes an explicit edit
/// over the request range, and a request range the map cannot carry (into a
/// gap, or right after one it emptied) gets no presentation.
fn transform_color_presentation_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    region_end: Position,
    host_request_range: Range,
) -> Vec<ColorPresentation> {
    if response_has_jsonrpc_error(&response, "textDocument/colorPresentation") {
        return vec![];
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        return vec![];
    };

    if result.is_null() {
        return vec![];
    }
    // Every presentation replaces the request range (its label, or an edit
    // the server keys to that range); over a gap of a prepared document that
    // range stands for host-owned text none may overwrite — the prepared
    // range may even be empty there, where an edit would only insert beside
    // the host text it was asked to replace. (The round trip below refuses
    // these too; this says why, and spares the probe.)
    if host_range_in_prepared_gap(host_request_range, offset) {
        return vec![];
    }
    // Nor may the request range map, as the edit replacing it, elsewhere
    // than where the client asked: a range empty right after a gap the peer
    // emptied reaches the server where the gap's start does, and an edit
    // there (the label's, or one the server keys to the range it got) maps
    // back before the gap's host text.
    if offset.prepared().is_some() {
        // A color's text: within one line, never clearing it.
        let mut probe = [tower_lsp_server::ls_types::TextEdit {
            range: host_request_range,
            new_text: "#".to_string(),
        }];
        translate_host_range_to_virtual(&mut probe[0].range, offset);
        if !translate_virtual_text_edits_to_host(&mut probe, offset, EditMode::Extent)
            || probe[0].range != host_request_range
        {
            return vec![];
        }
    }

    // Parse into typed Vec<ColorPresentation>
    let mut presentations: Vec<ColorPresentation> = match serde_json::from_value(result) {
        Ok(presentations) => presentations,
        Err(_) => return vec![],
    };

    // Transform textEdit and additionalTextEdits ranges to host coordinates,
    // then drop any presentation whose edits are unsafe for the injection
    // region (escape it, break line prefixes, or merge content into the
    // closing fence) if applied verbatim. Unlike completion, the
    // primary textEdit is not separable from the presentation (without it the
    // editor falls back to inserting the label, still at the unguarded
    // range), so an unsafe textEdit drops the whole presentation; unsafe
    // additionalTextEdits are merely stripped.
    let before = presentations.len();
    presentations.retain_mut(|presentation| {
        // No textEdit: the client replaces the REQUEST range with the label
        // (LSP 3.18). Guard that implicit edit exactly like an explicit one —
        // a synthetic TextEdit over the (host) request range with the label
        // as newText. Labels are single-line and request ranges in-region in
        // practice, so this rarely bites.
        // On a prepared document the label must also map through it like
        // any edit (a removed indent, a hidden line break beside it): the
        // implicit edit is made explicit, in prepared coordinates.
        if presentation.text_edit.is_none() {
            if offset.prepared().is_some() {
                let mut range = host_request_range;
                translate_host_range_to_virtual(&mut range, offset);
                presentation.text_edit = Some(tower_lsp_server::ls_types::TextEdit {
                    range,
                    new_text: presentation.label.clone(),
                });
            } else {
                let implicit = tower_lsp_server::ls_types::TextEdit {
                    range: host_request_range,
                    new_text: presentation.label.clone(),
                };
                if !text_edit_safe_in_region(&implicit, offset, region_end) {
                    return false;
                }
            }
        }
        // An unprepared document's additional edits are translated only once
        // the primary is accepted, below, as on main: no work for a dropped
        // presentation. A prepared one's map with the primary as one set.
        let prepared = offset.prepared().is_some();
        let Some(mut additional_kept) = translate_virtual_main_and_additional_edits_to_host(
            presentation.text_edit.as_mut_slice(),
            presentation.additional_text_edits.as_mut().filter(|_| prepared),
            offset,
            EditMode::Extent,
        ) else {
            return false;
        };
        if let Some(text_edit) = &presentation.text_edit
            && !text_edit_safe_in_region(text_edit, offset, region_end)
        {
            return false;
        }
        if !prepared && let Some(additional_edits) = presentation.additional_text_edits.as_mut() {
            additional_kept =
                translate_virtual_text_edits_to_host(additional_edits, offset, EditMode::Extent);
        }

        // ALL-OR-NOTHING (same reasoning as completion): the array can carry
        // paired halves of one operation, so any unsafe member drops the
        // whole array rather than half-applying it. The presentation is kept:
        // its textEdit still applies, though possibly semantically incomplete
        // — availability over fidelity, never corruption.
        if let Some(additional_edits) = &presentation.additional_text_edits
            && (!additional_kept
            || !additional_edits
                .iter()
                .all(|edit| text_edit_safe_in_region(edit, offset, region_end)))
        {
            log::warn!(
                target: "kakehashi::bridge",
                "colorPresentation: dropped a presentation's additionalTextEdits ({}): a member is unsafe for the injection region",
                additional_edits.len()
            );
            presentation.additional_text_edits = None;
        }
        true
    });
    let dropped = before - presentations.len();
    if dropped > 0 {
        log::warn!(
            target: "kakehashi::bridge",
            "colorPresentation: dropped {dropped} presentation(s) whose (implicit or explicit) textEdit is unsafe for the injection region"
        );
    }

    presentations
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::TEST_REGION_END;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    /// In-region host request range used by tests that don't exercise the
    /// implicit label-replacement guard (single position on the region line).
    fn test_host_request_range(region_start_line: u32) -> Range {
        Range {
            start: Position {
                line: region_start_line,
                character: 0,
            },
            end: Position {
                line: region_start_line,
                character: 4,
            },
        }
    }

    // ==========================================================================
    // Color presentation request tests
    // ==========================================================================

    #[test]
    fn color_presentation_request_uses_virtual_uri() {
        use tower_lsp_server::ls_types::Position;
        use url::Url;

        let host_uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let host_range = Range {
            start: Position {
                line: 5,
                character: 10,
            },
            end: Position {
                line: 5,
                character: 17,
            },
        };
        let color = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            color,
            &RegionOffset::new(3, 0),
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        let uri_str = json["params"]["textDocument"]["uri"].as_str().unwrap();
        assert!(
            VirtualDocumentUri::is_virtual_uri(uri_str),
            "Request should use a virtual URI: {}",
            uri_str
        );
        assert!(
            uri_str.ends_with(".lua"),
            "Virtual URI should have .lua extension: {}",
            uri_str
        );
    }

    #[test]
    fn color_presentation_request_transforms_range_to_virtual_coordinates() {
        use tower_lsp_server::ls_types::Position;
        use url::Url;

        let host_uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        // Host range: line 5, region starts at line 3
        // Virtual range should be: line 2 (5-3=2)
        let host_range = Range {
            start: Position {
                line: 5,
                character: 10,
            },
            end: Position {
                line: 5,
                character: 17,
            },
        };
        let color = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            color,
            &RegionOffset::new(3, 0),
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        let range = &json["params"]["range"];
        assert_eq!(
            range["start"]["line"], 2,
            "Start line should be translated from 5 to 2 (5-3)"
        );
        assert_eq!(
            range["start"]["character"], 10,
            "Start character should remain unchanged"
        );
        assert_eq!(
            range["end"]["line"], 2,
            "End line should be translated from 5 to 2 (5-3)"
        );
        assert_eq!(
            range["end"]["character"], 17,
            "End character should remain unchanged"
        );
    }

    #[test]
    fn color_presentation_request_includes_color() {
        use tower_lsp_server::ls_types::Position;
        use url::Url;

        let host_uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let host_range = Range {
            start: Position {
                line: 3,
                character: 0,
            },
            end: Position {
                line: 3,
                character: 7,
            },
        };
        let color = Color {
            red: 0.5,
            green: 0.25,
            blue: 0.75,
            alpha: 1.0,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            color,
            &RegionOffset::new(3, 0),
            RequestId::new(42),
        );

        insta::assert_json_snapshot!(request);
    }

    #[test]
    fn color_presentation_request_first_line_applies_column_offset() {
        use tower_lsp_server::ls_types::Position;
        use url::Url;

        let host_uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        // Host range on first line of region (line 3), region starts at col 5
        let host_range = Range {
            start: Position {
                line: 3,
                character: 10,
            },
            end: Position {
                line: 3,
                character: 17,
            },
        };
        let color = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            color,
            &RegionOffset::new(3, 5),
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        let range = &json["params"]["range"];
        // Virtual line 0 -> character adjusted: 10 - 5 = 5, 17 - 5 = 12
        assert_eq!(range["start"]["line"], 0);
        assert_eq!(range["start"]["character"], 5);
        assert_eq!(range["end"]["line"], 0);
        assert_eq!(range["end"]["character"], 12);
    }

    #[test]
    fn color_presentation_request_non_first_line_ignores_column_offset() {
        use tower_lsp_server::ls_types::Position;
        use url::Url;

        let host_uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        // Host range on non-first line of region
        let host_range = Range {
            start: Position {
                line: 5,
                character: 10,
            },
            end: Position {
                line: 5,
                character: 17,
            },
        };
        let color = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            color,
            &RegionOffset::new(3, 5),
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        let range = &json["params"]["range"];
        // Virtual line 2 -> character unchanged
        assert_eq!(range["start"]["line"], 2);
        assert_eq!(range["start"]["character"], 10);
        assert_eq!(range["end"]["line"], 2);
        assert_eq!(range["end"]["character"], 17);
    }

    #[test]
    fn color_presentation_request_range_saturates_on_underflow() {
        use tower_lsp_server::ls_types::Position;
        use url::Url;

        let host_uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        // Simulate race condition: range lines < region_start_line
        let host_range = Range {
            start: Position {
                line: 2,
                character: 5,
            },
            end: Position {
                line: 2,
                character: 12,
            },
        };
        let color = Color {
            red: 1.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        };

        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            color,
            &RegionOffset::new(10, 0), // region_start_line > range lines
            RequestId::new(42),
        );

        let json = serde_json::to_value(&request).unwrap();
        let range = &json["params"]["range"];
        assert_eq!(
            range["start"]["line"], 0,
            "Start line underflow should saturate to 0"
        );
        assert_eq!(
            range["end"]["line"], 0,
            "End line underflow should saturate to 0"
        );
    }

    // ==========================================================================
    // Color presentation response transformation tests
    // ==========================================================================

    #[test]
    fn color_presentation_response_transforms_text_edit_range_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "label": "#ff0000",
                    "textEdit": {
                        "range": {
                            "start": { "line": 0, "character": 10 },
                            "end": { "line": 0, "character": 17 }
                        },
                        "newText": "#ff0000"
                    }
                }
            ]
        });
        let region_start_line = 5;

        let presentations = transform_color_presentation_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            test_host_request_range(region_start_line),
        );

        assert_eq!(presentations.len(), 1);
        let text_edit = presentations[0].text_edit.as_ref().unwrap();
        assert_eq!(text_edit.range.start.line, 5);
        assert_eq!(text_edit.range.end.line, 5);
        assert_eq!(presentations[0].label, "#ff0000");
        assert_eq!(text_edit.new_text, "#ff0000");
    }

    #[test]
    fn color_presentation_response_transforms_additional_text_edits_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "rgb(255, 0, 0)",
                "textEdit": {
                    "range": {
                        "start": { "line": 2, "character": 5 },
                        "end": { "line": 2, "character": 12 }
                    },
                    "newText": "rgb(255, 0, 0)"
                },
                "additionalTextEdits": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "import { rgb } from 'colors';\n"
                    },
                    {
                        "range": {
                            "start": { "line": 4, "character": 0 },
                            "end": { "line": 4, "character": 10 }
                        },
                        "newText": "cleanup()"
                    }
                ]
            }]
        });
        let region_start_line = 3;

        let presentations = transform_color_presentation_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            test_host_request_range(region_start_line),
        );

        assert_eq!(presentations.len(), 1);
        let text_edit = presentations[0].text_edit.as_ref().unwrap();
        assert_eq!(text_edit.range.start.line, 5);
        assert_eq!(text_edit.range.end.line, 5);

        let additional = presentations[0].additional_text_edits.as_ref().unwrap();
        assert_eq!(additional.len(), 2);
        assert_eq!(additional[0].range.start.line, 3);
        assert_eq!(additional[0].range.end.line, 3);
        assert_eq!(additional[1].range.start.line, 7);
        assert_eq!(additional[1].range.end.line, 7);
    }

    #[test]
    fn color_presentation_response_without_text_edit_passes_through() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "label": "#ff0000" },
                { "label": "rgb(255, 0, 0)" },
                { "label": "hsl(0, 100%, 50%)" }
            ]
        });
        let region_start_line = 5;

        let presentations = transform_color_presentation_response_to_host(
            response,
            &RegionOffset::new(region_start_line, 0),
            TEST_REGION_END,
            test_host_request_range(region_start_line),
        );

        assert_eq!(presentations.len(), 3);
        assert_eq!(presentations[0].label, "#ff0000");
        assert_eq!(presentations[1].label, "rgb(255, 0, 0)");
        assert_eq!(presentations[2].label, "hsl(0, 100%, 50%)");
        // Without a map the label stays implicit, as main sends it: only a
        // prepared document's is made an explicit edit.
        assert!(
            presentations
                .iter()
                .all(|presentation| presentation.text_edit.is_none())
        );
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn color_presentation_response_returns_empty_for_invalid_response(
        #[case] response: serde_json::Value,
    ) {
        let presentations = transform_color_presentation_response_to_host(
            response,
            &RegionOffset::new(5, 0),
            TEST_REGION_END,
            test_host_request_range(5),
        );
        assert!(presentations.is_empty());
    }

    #[test]
    fn color_presentation_response_with_empty_array_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let presentations = transform_color_presentation_response_to_host(
            response,
            &RegionOffset::new(5, 0),
            TEST_REGION_END,
            test_host_request_range(5),
        );
        assert!(presentations.is_empty());
    }

    #[test]
    fn color_presentation_drops_prefix_breaking_presentations() {
        // Blockquote region, content host lines 3-4, region end (5, 0). A
        // presentation whose textEdit spans prefixed lines is dropped whole
        // (the label fallback would insert at the same unsafe range); an
        // unsafe additionalTextEdit is stripped from a kept presentation.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                { "label": "unsafe",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 1, "character": 2 } },
                                "newText": "#fff" } },
                { "label": "safe",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 4 } },
                                "newText": "#fff" },
                  "additionalTextEdits": [
                      { "range": { "start": { "line": 1, "character": 3 },
                                   "end": { "line": 1, "character": 5 } },
                        "newText": "safe-sibling" },
                      { "range": { "start": { "line": 1, "character": 0 },
                                   "end": { "line": 1, "character": 0 } },
                        "newText": "x\n" }
                  ] }
            ]
        });

        let presentations = transform_color_presentation_response_to_host(
            response,
            &offset,
            region_end,
            test_host_request_range(3),
        );

        assert_eq!(presentations.len(), 1, "unsafe presentation is dropped");
        assert_eq!(presentations[0].label, "safe");
        assert!(
            presentations[0].additional_text_edits.is_none(),
            "an unsafe member drops the whole additionalTextEdits array"
        );
    }

    #[test]
    fn color_presentation_drops_presentations_whose_edit_escapes_the_region() {
        // Plain fenced region (all-zero offsets), region end (5, 0): a
        // textEdit range translating past the closing fence must drop the
        // presentation even though the prefix rules fast-path.
        let offset = RegionOffset::with_per_line_offsets(3, vec![0, 0, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [
                { "label": "escapes",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 9, "character": 0 } },
                                "newText": "#fff" } },
                { "label": "contained",
                  "textEdit": { "range": { "start": { "line": 0, "character": 0 },
                                           "end": { "line": 0, "character": 4 } },
                                "newText": "#fff" } }
            ]
        });

        let presentations = transform_color_presentation_response_to_host(
            response,
            &offset,
            region_end,
            test_host_request_range(3),
        );

        assert_eq!(presentations.len(), 1, "region-escaping presentation drops");
        assert_eq!(presentations[0].label, "contained");
    }

    #[test]
    fn a_presentation_over_a_prepared_gap_is_dropped() {
        use super::super::super::protocol::{SegmentKind, VirtualLayout, apply_prepare_result};
        // `color: #${c};` prepared as `color: #ff0000;`, or with the
        // interpolation emptied (the request range then empty in P).
        let virtual_text = "color: #    ;";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..8, String::new()),
                (SegmentKind::Gap, 8..12, "${c}".to_string()),
                (SegmentKind::Content, 12..13, String::new()),
            ],
        );
        for placeholder in ["ff0000", ""] {
            let result = serde_json::from_value(json!({"segments": [
                {"type": "content"}, {"type": "gap", "content": placeholder}, {"type": "content"}
            ]}))
            .unwrap();
            let map = apply_prepare_result(virtual_text, &layout, Some(result))
                .unwrap()
                .map;
            let offset = RegionOffset::new(0, 0).with_prepared(map);
            let response = json!({"jsonrpc": "2.0", "id": 1, "result": [
                {"label": "#00ff00"},
                {"label": "x", "textEdit": {
                    "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
                    "newText": "shade"
                }}
            ]});
            // The editor's request range is the interpolation itself: every
            // presentation stands for replacing it.
            let kept = transform_color_presentation_response_to_host(
                response.clone(),
                &offset,
                Position::new(0, 13),
                Range::new(Position::new(0, 8), Position::new(0, 12)),
            );
            assert!(kept.is_empty(), "{placeholder:?}: {kept:?}");
            // Requested beside it, both map: the label as an explicit edit.
            let kept = transform_color_presentation_response_to_host(
                response,
                &offset,
                Position::new(0, 13),
                Range::new(Position::new(0, 0), Position::new(0, 5)),
            );
            assert_eq!(kept.len(), 2, "{placeholder:?}: {kept:?}");
            assert_eq!(
                kept[0].text_edit,
                Some(tower_lsp_server::ls_types::TextEdit {
                    range: Range::new(Position::new(0, 0), Position::new(0, 5)),
                    new_text: "#00ff00".to_string(),
                })
            );
        }
    }

    #[test]
    fn a_label_mapped_away_from_its_request_range_is_dropped() {
        use super::super::super::protocol::{SegmentKind, VirtualLayout, apply_prepare_result};
        // `color: #${c};` with the interpolation emptied: a request range
        // empty right after it reaches the server where its start does.
        let virtual_text = "color: #    ;";
        let layout = VirtualLayout::from_pieces(
            virtual_text,
            [
                (SegmentKind::Content, 0..8, String::new()),
                (SegmentKind::Gap, 8..12, "${c}".to_string()),
                (SegmentKind::Content, 12..13, String::new()),
            ],
        );
        let result = serde_json::from_value(json!({"segments": [
            {"type": "content"}, {"type": "gap", "content": ""}, {"type": "content"}
        ]}))
        .unwrap();
        let map = apply_prepare_result(virtual_text, &layout, Some(result))
            .unwrap()
            .map;
        let offset = RegionOffset::new(0, 0).with_prepared(map);
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": [{"label": "#00ff00"}]});
        let at_gap_end = Range::new(Position::new(0, 12), Position::new(0, 12));
        let kept = transform_color_presentation_response_to_host(
            response.clone(),
            &offset,
            Position::new(0, 13),
            at_gap_end,
        );
        assert!(kept.is_empty(), "{kept:?}");
        // So is an edit the server keys to the range it got.
        let keyed = json!({"jsonrpc": "2.0", "id": 1, "result": [{"label": "#00ff00", "textEdit": {
            "range": {"start": {"line": 0, "character": 8}, "end": {"line": 0, "character": 8}},
            "newText": "#00ff00"
        }}]});
        let kept = transform_color_presentation_response_to_host(
            keyed,
            &offset,
            Position::new(0, 13),
            at_gap_end,
        );
        assert!(kept.is_empty(), "{kept:?}");
        // Over the text after the gap, it maps where it was asked.
        let after_gap = Range::new(Position::new(0, 12), Position::new(0, 13));
        let kept = transform_color_presentation_response_to_host(
            response,
            &offset,
            Position::new(0, 13),
            after_gap,
        );
        assert_eq!(
            kept[0].text_edit.as_ref().map(|edit| edit.range),
            Some(after_gap)
        );
    }

    #[test]
    fn an_answer_whose_map_is_no_longer_current_is_dropped() {
        let offset = dedented_offset();
        let host_uri: tower_lsp_server::ls_types::Uri = "file:///test.md".parse().unwrap();
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": [{"label": "x", "textEdit": {
            "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}},
            "newText": "c"
        }}]});
        let answer = |current: bool| {
            let ctx = super::super::super::pool::BridgeResponseContext {
                virtual_uri_string: "file:///test.md/v.lua".to_string(),
                host_uri_lsp: &host_uri,
                offset: &offset,
                request_map_current: current,
            };
            color_presentation_response_to_host(
                response.clone(),
                &ctx,
                Position::new(2, 0),
                Range::new(Position::new(1, 2), Position::new(1, 3)),
            )
        };
        assert_eq!(answer(true).len(), 1);
        assert!(answer(false).is_empty());
    }

    /// `  a\n  b\n`, dedented by two.
    fn dedented_offset() -> RegionOffset {
        use super::super::super::protocol::{VirtualLayout, apply_prepare_result};
        let result = serde_json::from_value(json!({"segments": [{"type": "content", "changes": [
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": ""},
            {"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 2}}, "newText": ""}
        ]}]}))
        .unwrap();
        let map = apply_prepare_result(
            "  a\n  b\n",
            &VirtualLayout::single("  a\n  b\n"),
            Some(result),
        )
        .unwrap()
        .map;
        RegionOffset::new(0, 0).with_prepared(map)
    }

    #[test]
    fn a_prepared_presentations_edits_map_and_reindent() {
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": [{
            "label": "x",
            "textEdit": {
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}},
                "newText": "c\nd"
            },
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
                "newText": "z"
            }]
        }]});
        let kept = transform_color_presentation_response_to_host(
            response,
            &dedented_offset(),
            Position::new(2, 0),
            Range::new(Position::new(1, 2), Position::new(1, 3)),
        );
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(
            kept[0].text_edit,
            Some(tower_lsp_server::ls_types::TextEdit {
                range: Range::new(Position::new(1, 2), Position::new(1, 3)),
                new_text: "c\n  d".to_string(),
            })
        );
        assert_eq!(
            kept[0].additional_text_edits,
            Some(vec![tower_lsp_server::ls_types::TextEdit {
                range: Range::new(Position::new(0, 2), Position::new(0, 2)),
                new_text: "z".to_string(),
            }])
        );
    }

    #[test]
    fn additional_edits_refused_with_the_main_edit_are_dropped() {
        // The additional edit joins the main edit's line onto the one
        // before: touching it at a line start, the set is refused, and the
        // main edit stays as it maps alone.
        let response = json!({"jsonrpc": "2.0", "id": 1, "result": [{
            "label": "x",
            "textEdit": {
                "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 1}},
                "newText": "c"
            },
            "additionalTextEdits": [{
                "range": {"start": {"line": 0, "character": 1}, "end": {"line": 1, "character": 0}},
                "newText": ""
            }]
        }]});
        let kept = transform_color_presentation_response_to_host(
            response,
            &dedented_offset(),
            Position::new(2, 0),
            Range::new(Position::new(1, 2), Position::new(1, 3)),
        );
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(kept[0].additional_text_edits, None);
        assert_eq!(
            kept[0].text_edit.as_ref().map(|edit| edit.range),
            Some(Range::new(Position::new(1, 2), Position::new(1, 3)))
        );
    }

    #[test]
    fn color_presentation_drops_label_fallback_over_a_prefixed_multiline_range() {
        // No textEdit: the client replaces the REQUEST range with the label.
        // A request range spanning prefixed lines makes even a single-line
        // label strip the second line's `> ` prefix — the synthetic implicit
        // edit must be rejected like an explicit one.
        let offset = RegionOffset::with_per_line_offsets(3, vec![2, 2, 0]);
        let region_end = Position {
            line: 5,
            character: 0,
        };
        let multiline_request_range = Range {
            start: Position {
                line: 3,
                character: 2,
            },
            end: Position {
                line: 4,
                character: 4,
            },
        };
        let response = json!({
            "jsonrpc": "2.0", "id": 42,
            "result": [ { "label": "#ff0000" } ]
        });

        let presentations = transform_color_presentation_response_to_host(
            response,
            &offset,
            region_end,
            multiline_request_range,
        );

        assert!(
            presentations.is_empty(),
            "label fallback over a prefixed multi-line range must drop: {presentations:?}"
        );
    }
}
