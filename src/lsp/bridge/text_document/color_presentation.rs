//! Color presentation requests for downstream servers, handling the bidirectional
//! host↔virtual coordinate transformation. Like inlay hints, the request carries a
//! range (where the color was found) and the response may include textEdits and
//! additionalTextEdits whose ranges must be translated back to host coordinates.
//! Presentations whose textEdit would break per-line region prefixes
//! (blockquote `> `) are dropped; unsafe additionalTextEdits are stripped.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{
    Color, ColorPresentation, ColorPresentationParams, Position, Range, TextDocumentIdentifier,
};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, region_host_end,
    response_has_jsonrpc_error, text_edit_safe_in_region, translate_host_range_to_virtual,
    translate_virtual_range_to_host, virtual_uri_to_lsp_uri,
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
        color: Color,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Vec<ColorPresentation>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
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
                let region_end = region_host_end(virtual_content, ctx.offset);
                transform_color_presentation_response_to_host(response, ctx.offset, region_end)
            },
        )
        .await
    }
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
/// translating the ranges of each item's `textEdit` and `additionalTextEdits`
/// while leaving the label unchanged.
fn transform_color_presentation_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
    region_end: Position,
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

    // Parse into typed Vec<ColorPresentation>
    let mut presentations: Vec<ColorPresentation> = match serde_json::from_value(result) {
        Ok(presentations) => presentations,
        Err(_) => return vec![],
    };

    // Transform textEdit and additionalTextEdits ranges to host coordinates,
    // then drop any presentation whose edits would break region line prefixes
    // (e.g. blockquote `> `) if applied verbatim. Unlike completion, the
    // primary textEdit is not separable from the presentation (without it the
    // editor falls back to inserting the label, still at the unguarded
    // range), so an unsafe textEdit drops the whole presentation; unsafe
    // additionalTextEdits are merely stripped.
    let region_prefixed = offset.columns().iter().any(|column| *column != 0);
    let before = presentations.len();
    presentations.retain_mut(|presentation| {
        // No textEdit: the client inserts the LABEL at the request range. A
        // multi-line label in a prefixed region is the same hazard as a
        // multi-line textEdit (no range to check here — the newline is the
        // signal). Labels are single-line in practice, so this never bites.
        if presentation.text_edit.is_none()
            && region_prefixed
            && (presentation.label.contains('\n') || presentation.label.contains('\r'))
        {
            return false;
        }
        if let Some(text_edit) = &mut presentation.text_edit {
            translate_virtual_range_to_host(&mut text_edit.range, offset);
            if !text_edit_safe_in_region(text_edit, offset, region_end) {
                return false;
            }
        }

        if let Some(additional_edits) = &mut presentation.additional_text_edits {
            let before = additional_edits.len();
            for edit in additional_edits.iter_mut() {
                translate_virtual_range_to_host(&mut edit.range, offset);
            }
            additional_edits
                .retain(|edit| text_edit_safe_in_region(edit, offset, region_end));
            let stripped = before - additional_edits.len();
            if stripped > 0 {
                log::warn!(
                    target: "kakehashi::bridge",
                    "colorPresentation: stripped {stripped} additionalTextEdit(s) that would break region line prefixes"
                );
            }
        }
        true
    });
    let dropped = before - presentations.len();
    if dropped > 0 {
        log::warn!(
            target: "kakehashi::bridge",
            "colorPresentation: dropped {dropped} presentation(s) whose textEdit would break region line prefixes"
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
        );

        assert_eq!(presentations.len(), 3);
        assert_eq!(presentations[0].label, "#ff0000");
        assert_eq!(presentations[1].label, "rgb(255, 0, 0)");
        assert_eq!(presentations[2].label, "hsl(0, 100%, 50%)");
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
                      { "range": { "start": { "line": 1, "character": 0 },
                                   "end": { "line": 1, "character": 0 } },
                        "newText": "x\n" }
                  ] }
            ]
        });

        let presentations =
            transform_color_presentation_response_to_host(response, &offset, region_end);

        assert_eq!(presentations.len(), 1, "unsafe presentation is dropped");
        assert_eq!(presentations[0].label, "safe");
        assert_eq!(
            presentations[0]
                .additional_text_edits
                .as_ref()
                .map(Vec::len),
            Some(0),
            "prefix-breaking additionalTextEdit is stripped"
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

        let presentations =
            transform_color_presentation_response_to_host(response, &offset, region_end);

        assert_eq!(presentations.len(), 1, "region-escaping presentation drops");
        assert_eq!(presentations[0].label, "contained");
    }
}
