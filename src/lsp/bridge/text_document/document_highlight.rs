//! Document highlight request handling for bridge connections.
//!
//! This module provides document highlight request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{DocumentHighlight, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_position_based_request};

impl LanguageServerPool {
    /// Send a document highlight request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing document-highlight-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_document_highlight_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: UpstreamId,
    ) -> io::Result<Option<Vec<DocumentHighlight>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/documentHighlight") {
            return Ok(None);
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
                build_document_highlight_request(
                    virtual_uri,
                    host_position,
                    region_start_line,
                    request_id,
                )
            },
            |response, ctx| {
                transform_document_highlight_response_to_host(response, ctx.region_start_line)
            },
        )
        .await
    }
}

/// Build a JSON-RPC document highlight request for a downstream language server.
///
/// This is a thin wrapper around `build_position_based_request` with the method
/// name "textDocument/documentHighlight".
fn build_document_highlight_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: tower_lsp_server::ls_types::Position,
    region_start_line: u32,
    request_id: RequestId,
) -> serde_json::Value {
    build_position_based_request(
        virtual_uri,
        host_position,
        region_start_line,
        request_id,
        "textDocument/documentHighlight",
    )
}

/// Transform a document highlight response from virtual to host document coordinates.
///
/// DocumentHighlight responses are arrays of items with range and optional kind.
/// This function transforms each range's line numbers by adding region_start_line.
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `region_start_line` - The starting line of the injection region in the host document
fn transform_document_highlight_response_to_host(
    mut response: serde_json::Value,
    region_start_line: u32,
) -> Option<Vec<DocumentHighlight>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/documentHighlight: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse into typed Vec<DocumentHighlight>
    let mut highlights: Vec<DocumentHighlight> = serde_json::from_value(result).ok()?;

    // Transform ranges to host coordinates
    for highlight in &mut highlights {
        highlight.range.start.line = highlight.range.start.line.saturating_add(region_start_line);
        highlight.range.end.line = highlight.range.end.line.saturating_add(region_start_line);
    }

    Some(highlights)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::VirtualDocumentUri;
    use serde_json::json;

    #[test]
    fn document_highlight_request_uses_virtual_uri() {
        use tower_lsp_server::ls_types::{Position, Uri};
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let position = Position {
            line: 5,
            character: 10,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request =
            build_document_highlight_request(&virtual_uri, position, 3, RequestId::new(42));

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
    fn document_highlight_request_translates_position_to_virtual_coordinates() {
        use tower_lsp_server::ls_types::{Position, Uri};
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        // Host line 5, region starts at line 3 -> virtual line 2
        let position = Position {
            line: 5,
            character: 10,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request =
            build_document_highlight_request(&virtual_uri, position, 3, RequestId::new(42));

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 42);
        assert_eq!(request["method"], "textDocument/documentHighlight");
        assert_eq!(request["params"]["position"]["line"], 2);
        assert_eq!(request["params"]["position"]["character"], 10);
    }

    #[test]
    fn document_highlight_response_transforms_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 6 },
                        "end": { "line": 0, "character": 11 }
                    },
                    "kind": 1
                },
                {
                    "range": {
                        "start": { "line": 2, "character": 0 },
                        "end": { "line": 2, "character": 5 }
                    },
                    "kind": 2
                },
                {
                    "range": {
                        "start": { "line": 4, "character": 0 },
                        "end": { "line": 4, "character": 5 }
                    }
                }
            ]
        });
        let region_start_line = 3;

        let transformed =
            transform_document_highlight_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let highlights = transformed.unwrap();
        assert_eq!(highlights.len(), 3);
        assert_eq!(highlights[0].range.start.line, 3);
        assert_eq!(highlights[0].range.end.line, 3);
        assert!(highlights[0].kind.is_some());
        assert_eq!(highlights[1].range.start.line, 5);
        assert_eq!(highlights[1].range.end.line, 5);
        assert!(highlights[1].kind.is_some());
        assert_eq!(highlights[2].range.start.line, 7);
    }

    #[test]
    fn document_highlight_response_with_null_result_returns_none() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let transformed = transform_document_highlight_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn document_highlight_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed = transform_document_highlight_response_to_host(response, 5);
        assert!(transformed.is_some());
        let highlights = transformed.unwrap();
        assert!(highlights.is_empty());
    }

    #[test]
    fn document_highlight_response_with_no_result_key_returns_none() {
        // JSON-RPC error response has no "result" key
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });

        let transformed = transform_document_highlight_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn document_highlight_response_with_malformed_result_returns_none() {
        // Result is a string instead of a DocumentHighlight array
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_an_array"
        });

        let transformed = transform_document_highlight_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn document_highlight_response_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": u32::MAX, "character": 0 },
                        "end": { "line": u32::MAX, "character": 5 }
                    },
                    "kind": 1
                }
            ]
        });
        let region_start_line = 10;

        let transformed =
            transform_document_highlight_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let highlights = transformed.unwrap();
        assert_eq!(highlights.len(), 1);
        assert_eq!(
            highlights[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
        assert_eq!(
            highlights[0].range.end.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }

    #[test]
    fn document_highlight_response_preserves_character_coordinates() {
        // Character coordinates should not be transformed, only line numbers
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 15 },
                        "end": { "line": 1, "character": 20 }
                    }
                }
            ]
        });
        let region_start_line = 10;

        let transformed =
            transform_document_highlight_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let highlights = transformed.unwrap();
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].range.start.line, 10); // 0 + 10
        assert_eq!(highlights[0].range.start.character, 15); // Preserved
        assert_eq!(highlights[0].range.end.line, 11); // 1 + 10
        assert_eq!(highlights[0].range.end.character, 20); // Preserved
    }
}
