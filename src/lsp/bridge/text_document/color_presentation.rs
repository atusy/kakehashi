//! Color presentation request handling for bridge connections.
//!
//! This module provides color presentation request functionality for downstream language servers,
//! handling the bidirectional coordinate transformation between host and virtual documents.
//!
//! Like inlay hints, color presentation uses a range parameter in the request (the range
//! where the color was found) and the response may contain textEdits and additionalTextEdits
//! that need transformation back to host coordinates.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{ColorPresentation, Range};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri};

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
        color: &serde_json::Value,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Vec<ColorPresentation>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/colorPresentation") {
            return Ok(vec![]);
        }
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            region_start_line,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_color_presentation_request(
                    virtual_uri,
                    host_range,
                    color,
                    region_start_line,
                    request_id,
                )
            },
            |response, ctx| {
                transform_color_presentation_response_to_host(response, ctx.region_start_line)
            },
        )
        .await
    }
}

/// Build a JSON-RPC color presentation request for a downstream language server.
///
/// ColorPresentationParams has a range field that specifies the color's location
/// in the document. This range needs to be translated from host to virtual coordinates.
///
/// # Defensive Arithmetic
///
/// Uses `saturating_sub` for line translation to prevent panic on underflow during
/// race conditions when document edits invalidate region data.
fn build_color_presentation_request(
    virtual_uri: &VirtualDocumentUri,
    host_range: Range,
    color: &serde_json::Value,
    region_start_line: u32,
    request_id: RequestId,
) -> serde_json::Value {
    // Translate range from host to virtual coordinates
    // Uses saturating_sub to prevent panic on race conditions
    let virtual_range = Range {
        start: tower_lsp_server::ls_types::Position {
            line: host_range.start.line.saturating_sub(region_start_line),
            character: host_range.start.character,
        },
        end: tower_lsp_server::ls_types::Position {
            line: host_range.end.line.saturating_sub(region_start_line),
            character: host_range.end.character,
        },
    };

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": "textDocument/colorPresentation",
        "params": {
            "textDocument": {
                "uri": virtual_uri.to_uri_string()
            },
            "color": color,
            "range": {
                "start": {
                    "line": virtual_range.start.line,
                    "character": virtual_range.start.character
                },
                "end": {
                    "line": virtual_range.end.line,
                    "character": virtual_range.end.character
                }
            }
        }
    })
}

/// Transform a color presentation response from virtual to host document coordinates.
///
/// ColorPresentation responses are arrays of ColorPresentation items, each containing:
/// - label: The presentation label (preserved unchanged)
/// - textEdit: Optional TextEdit with range (needs transformation)
/// - additionalTextEdits: Optional array of TextEdits (ranges need transformation)
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `region_start_line` - The starting line of the injection region in the host document
fn transform_color_presentation_response_to_host(
    mut response: serde_json::Value,
    region_start_line: u32,
) -> Vec<ColorPresentation> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/colorPresentation: {}", error);
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

    // Transform textEdit and additionalTextEdits ranges to host coordinates
    for presentation in &mut presentations {
        if let Some(text_edit) = &mut presentation.text_edit {
            text_edit.range.start.line =
                text_edit.range.start.line.saturating_add(region_start_line);
            text_edit.range.end.line = text_edit.range.end.line.saturating_add(region_start_line);
        }

        if let Some(additional_edits) = &mut presentation.additional_text_edits {
            for edit in additional_edits.iter_mut() {
                edit.range.start.line = edit.range.start.line.saturating_add(region_start_line);
                edit.range.end.line = edit.range.end.line.saturating_add(region_start_line);
            }
        }
    }

    presentations
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let color = json!({
            "red": 1.0,
            "green": 0.0,
            "blue": 0.0,
            "alpha": 1.0
        });
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            &color,
            3,
            RequestId::new(42),
        );

        let uri_str = request["params"]["textDocument"]["uri"].as_str().unwrap();
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
        let color = json!({
            "red": 1.0,
            "green": 0.0,
            "blue": 0.0,
            "alpha": 1.0
        });
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            &color,
            3,
            RequestId::new(42),
        );

        let range = &request["params"]["range"];
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
        let color = json!({
            "red": 0.5,
            "green": 0.25,
            "blue": 0.75,
            "alpha": 1.0
        });
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            &color,
            3,
            RequestId::new(42),
        );

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 42);
        assert_eq!(request["method"], "textDocument/colorPresentation");
        assert_eq!(request["params"]["color"]["red"], 0.5);
        assert_eq!(request["params"]["color"]["green"], 0.25);
        assert_eq!(request["params"]["color"]["blue"], 0.75);
        assert_eq!(request["params"]["color"]["alpha"], 1.0);
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
        let color = json!({
            "red": 1.0,
            "green": 0.0,
            "blue": 0.0,
            "alpha": 1.0
        });

        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_color_presentation_request(
            &virtual_uri,
            host_range,
            &color,
            10, // region_start_line > range lines
            RequestId::new(42),
        );

        let range = &request["params"]["range"];
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

        let presentations =
            transform_color_presentation_response_to_host(response, region_start_line);

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

        let presentations =
            transform_color_presentation_response_to_host(response, region_start_line);

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

        let presentations =
            transform_color_presentation_response_to_host(response, region_start_line);

        assert_eq!(presentations.len(), 3);
        assert_eq!(presentations[0].label, "#ff0000");
        assert_eq!(presentations[1].label, "rgb(255, 0, 0)");
        assert_eq!(presentations[2].label, "hsl(0, 100%, 50%)");
    }

    #[test]
    fn color_presentation_response_with_null_result_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let presentations = transform_color_presentation_response_to_host(response, 5);
        assert!(presentations.is_empty());
    }

    #[test]
    fn color_presentation_response_with_empty_array_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let presentations = transform_color_presentation_response_to_host(response, 5);
        assert!(presentations.is_empty());
    }

    #[test]
    fn color_presentation_response_with_no_result_key_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });

        let presentations = transform_color_presentation_response_to_host(response, 5);
        assert!(presentations.is_empty());
    }

    #[test]
    fn color_presentation_response_with_malformed_result_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_an_array"
        });

        let presentations = transform_color_presentation_response_to_host(response, 5);
        assert!(presentations.is_empty());
    }
}
