//! Document color request handling for bridge connections.
//!
//! This module provides document color request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! Unlike position-based requests (hover, definition, etc.), document color requests
//! operate on the entire document - they don't take a position parameter.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::ColorInformation;
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_whole_document_request};

impl LanguageServerPool {
    /// Send a document color request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing document-color-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_document_color_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Vec<ColorInformation>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/documentColor") {
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
            build_document_color_request,
            |response, ctx| {
                transform_document_color_response_to_host(response, ctx.region_start_line)
            },
        )
        .await
    }
}

/// Build a JSON-RPC document color request for a downstream language server.
///
/// Like DocumentLinkParams, DocumentColorParams only has a textDocument field -
/// no position. The request asks for all colors in the entire document.
fn build_document_color_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> serde_json::Value {
    build_whole_document_request(virtual_uri, request_id, "textDocument/documentColor")
}

/// Transform a document color response from virtual to host document coordinates.
///
/// DocumentColor responses are arrays of ColorInformation items, each containing:
/// - range: The range where the color was found (needs transformation)
/// - color: The color value with RGBA components (preserved unchanged)
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `region_start_line` - The starting line of the injection region in the host document
fn transform_document_color_response_to_host(
    mut response: serde_json::Value,
    region_start_line: u32,
) -> Vec<ColorInformation> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/documentColor: {}", error);
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        return vec![];
    };

    if result.is_null() {
        return vec![];
    }

    // Parse into typed Vec<ColorInformation>
    let mut colors: Vec<ColorInformation> = match serde_json::from_value(result) {
        Ok(colors) => colors,
        Err(_) => return vec![],
    };

    // Transform ranges to host coordinates
    for color in &mut colors {
        color.range.start.line = color.range.start.line.saturating_add(region_start_line);
        color.range.end.line = color.range.end.line.saturating_add(region_start_line);
    }

    colors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::VirtualDocumentUri;
    use serde_json::json;

    // ==========================================================================
    // Document color request tests
    // ==========================================================================

    #[test]
    fn document_color_request_uses_virtual_uri() {
        use tower_lsp_server::ls_types::Uri;
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_document_color_request(&virtual_uri, RequestId::new(42));

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
    fn document_color_request_has_correct_method_and_no_position() {
        use tower_lsp_server::ls_types::Uri;
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_document_color_request(&virtual_uri, RequestId::new(123));

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 123);
        assert_eq!(request["method"], "textDocument/documentColor");
        assert!(
            request["params"].get("position").is_none(),
            "DocumentColor request should not have position parameter"
        );
    }

    // ==========================================================================
    // Document color response transformation tests
    // ==========================================================================

    #[test]
    fn document_color_response_transforms_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 10 },
                        "end": { "line": 0, "character": 17 }
                    },
                    "color": {
                        "red": 1.0,
                        "green": 0.0,
                        "blue": 0.0,
                        "alpha": 1.0
                    }
                },
                {
                    "range": {
                        "start": { "line": 2, "character": 5 },
                        "end": { "line": 2, "character": 12 }
                    },
                    "color": {
                        "red": 0.0,
                        "green": 1.0,
                        "blue": 0.0,
                        "alpha": 1.0
                    }
                }
            ]
        });
        let region_start_line = 5;

        let colors = transform_document_color_response_to_host(response, region_start_line);

        assert_eq!(colors.len(), 2);
        assert_eq!(colors[0].range.start.line, 5);
        assert_eq!(colors[0].range.end.line, 5);
        assert_eq!(colors[0].range.start.character, 10);
        assert_eq!(colors[0].color.red, 1.0);
        assert_eq!(colors[0].color.green, 0.0);
        assert_eq!(colors[1].range.start.line, 7);
        assert_eq!(colors[1].range.end.line, 7);
    }

    #[test]
    fn document_color_response_with_null_result_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let colors = transform_document_color_response_to_host(response, 5);
        assert!(colors.is_empty());
    }

    #[test]
    fn document_color_response_with_empty_array_returns_empty() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let colors = transform_document_color_response_to_host(response, 5);
        assert!(colors.is_empty());
    }

    #[test]
    fn document_color_response_preserves_color_values() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 7 }
                },
                "color": {
                    "red": 0.5,
                    "green": 0.25,
                    "blue": 0.75,
                    "alpha": 0.9
                }
            }]
        });
        let region_start_line = 3;

        let colors = transform_document_color_response_to_host(response, region_start_line);

        assert_eq!(colors.len(), 1);
        assert_eq!(colors[0].range.start.line, 3);
        assert_eq!(colors[0].color.red, 0.5);
        assert_eq!(colors[0].color.green, 0.25);
        assert_eq!(colors[0].color.blue, 0.75);
        assert_eq!(colors[0].color.alpha, 0.9);
    }

    #[test]
    fn document_color_response_with_no_result_key_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });

        let colors = transform_document_color_response_to_host(response, 5);
        assert!(colors.is_empty());
    }

    #[test]
    fn document_color_response_with_malformed_result_returns_empty() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_an_array"
        });

        let colors = transform_document_color_response_to_host(response, 5);
        assert!(colors.is_empty());
    }

    #[test]
    fn document_color_response_transformation_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                },
                "color": {
                    "red": 1.0,
                    "green": 0.0,
                    "blue": 0.0,
                    "alpha": 1.0
                }
            }]
        });
        let region_start_line = 10;

        let colors = transform_document_color_response_to_host(response, region_start_line);

        assert_eq!(colors.len(), 1);
        assert_eq!(
            colors[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }
}
