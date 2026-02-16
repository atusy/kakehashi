//! Hover request handling for bridge connections.
//!
//! This module provides hover request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{Hover, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_position_based_request};

impl LanguageServerPool {
    /// Send a hover request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing hover-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_hover_request(
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
    ) -> io::Result<Option<Hover>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/hover") {
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
                build_hover_request(virtual_uri, host_position, region_start_line, request_id)
            },
            |response, ctx| transform_hover_response_to_host(response, ctx.region_start_line),
        )
        .await
    }
}

/// Build a JSON-RPC hover request for a downstream language server.
fn build_hover_request(
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
        "textDocument/hover",
    )
}

/// Parse a JSON-RPC hover response and transform coordinates to host document space.
///
/// Instead of returning a modified JSON envelope, this deserializes the response
/// into `Option<Hover>` with coordinates already transformed.
///
/// Returns `None` for: null results, missing results, and deserialization failures.
///
/// # Arguments
/// * `response` - Raw JSON-RPC response envelope (`{"result": {...}}`)
/// * `region_start_line` - Line offset to add to hover range if present
fn transform_hover_response_to_host(
    mut response: serde_json::Value,
    region_start_line: u32,
) -> Option<Hover> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/hover: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    // Deserialize into typed Hover
    let Ok(mut hover) = serde_json::from_value::<Hover>(result) else {
        return None;
    };

    // Transform range if present
    if let Some(range) = &mut hover.range {
        // Uses saturating_add to prevent overflow, consistent with saturating_sub
        // used elsewhere in the codebase for defensive arithmetic
        range.start.line = range.start.line.saturating_add(region_start_line);
        range.end.line = range.end.line.saturating_add(region_start_line);
    }

    Some(hover)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp_server::ls_types::Position;
    use url::Url;

    // ==========================================================================
    // Test helpers
    // ==========================================================================

    /// Standard test request ID used across most tests.
    fn test_request_id() -> RequestId {
        RequestId::new(42)
    }

    /// Standard test host URI used across most tests.
    fn test_host_uri() -> tower_lsp_server::ls_types::Uri {
        let url = Url::parse("file:///project/doc.md").unwrap();
        crate::lsp::lsp_impl::url_to_uri(&url).expect("test URL should convert to URI")
    }

    /// Standard test position (line 5, character 10).
    fn test_position() -> Position {
        Position {
            line: 5,
            character: 10,
        }
    }

    /// Assert that a request uses a virtual URI with the expected extension.
    fn assert_uses_virtual_uri(request: &serde_json::Value, extension: &str) {
        let uri_str = request["params"]["textDocument"]["uri"].as_str().unwrap();
        // Use url crate for robust parsing (handles query strings with slashes, fragments, etc.)
        let url = url::Url::parse(uri_str).expect("URI should be parseable");
        let filename = url
            .path_segments()
            .and_then(|mut s| s.next_back())
            .unwrap_or("");
        assert!(
            filename.starts_with("kakehashi-virtual-uri-")
                && filename.ends_with(&format!(".{}", extension)),
            "Request should use virtual URI with .{} extension: {}",
            extension,
            uri_str
        );
    }

    /// Assert that a position-based request has correct structure and translated coordinates.
    fn assert_position_request(
        request: &serde_json::Value,
        expected_method: &str,
        expected_virtual_line: u64,
    ) {
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 42);
        assert_eq!(request["method"], expected_method);
        assert_eq!(
            request["params"]["position"]["line"], expected_virtual_line,
            "Position line should be translated"
        );
        assert_eq!(
            request["params"]["position"]["character"], 10,
            "Character should remain unchanged"
        );
    }

    // ==========================================================================
    // Hover request tests
    // ==========================================================================

    #[test]
    fn hover_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_hover_request(&virtual_uri, test_position(), 3, test_request_id());

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn hover_request_translates_position_to_virtual_coordinates() {
        // Host line 5, region starts at line 3 -> virtual line 2
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_hover_request(&virtual_uri, test_position(), 3, test_request_id());

        assert_position_request(&request, "textDocument/hover", 2);
    }

    #[test]
    fn position_translation_at_region_start_becomes_line_zero() {
        // When cursor is at the first line of the region, virtual line should be 0
        let host_position = Position {
            line: 3, // Same as region_start_line
            character: 5,
        };

        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_hover_request(&virtual_uri, host_position, 3, test_request_id());

        assert_eq!(
            request["params"]["position"]["line"], 0,
            "Position at region start should translate to line 0"
        );
    }

    #[test]
    fn position_translation_with_zero_region_start() {
        // Region starting at line 0 (e.g., first line of document)
        let host_position = Position {
            line: 5,
            character: 0,
        };

        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_hover_request(&virtual_uri, host_position, 0, test_request_id());

        assert_eq!(
            request["params"]["position"]["line"], 5,
            "With region_start_line=0, virtual line equals host line"
        );
    }

    #[test]
    fn position_translation_saturates_on_underflow() {
        // Simulate race condition: host_position.line (2) < region_start_line (5)
        // This should NOT panic, instead saturate to line 0
        let host_position = Position {
            line: 2, // Less than region_start_line
            character: 10,
        };

        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_hover_request(
            &virtual_uri,
            host_position,
            5, // region_start_line > host_position.line
            test_request_id(),
        );

        assert_eq!(
            request["params"]["position"]["line"], 0,
            "Underflow should saturate to line 0, not panic"
        );
        assert_eq!(
            request["params"]["position"]["character"], 10,
            "Character should remain unchanged"
        );
    }

    // ==========================================================================
    // Hover response transformation tests
    // ==========================================================================

    #[test]
    fn hover_response_transforms_range_to_host_coordinates() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "contents": {
                    "kind": "markdown",
                    "value": "docs"
                },
                "range": {
                    "start": { "line": 0, "character": 9 },
                    "end": { "line": 0, "character": 14 }
                }
            }
        });
        let region_start_line = 3;

        let transformed = transform_hover_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let hover = transformed.unwrap();
        assert!(hover.range.is_some());
        let range = hover.range.unwrap();
        assert_eq!(range.start.line, 3);
        assert_eq!(range.end.line, 3);
        assert_eq!(range.start.character, 9);
        assert_eq!(range.end.character, 14);
    }

    #[test]
    fn hover_response_without_range_passes_through() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "contents": "Simple hover text"
            }
        });

        let transformed = transform_hover_response_to_host(response, 5);
        assert!(transformed.is_some());
        let hover = transformed.unwrap();
        assert!(hover.range.is_none());
    }

    #[test]
    fn hover_response_with_null_result_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": null
        });

        let transformed = transform_hover_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn hover_response_transformation_with_zero_region_start() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "contents": { "kind": "markdown", "value": "docs" },
                "range": {
                    "start": { "line": 2, "character": 0 },
                    "end": { "line": 2, "character": 10 }
                }
            }
        });
        let region_start_line = 0;

        let transformed = transform_hover_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let hover = transformed.unwrap();
        let range = hover.range.unwrap();
        assert_eq!(
            range.start.line, 2,
            "With region_start_line=0, host line equals virtual line"
        );
    }

    #[test]
    fn hover_response_transformation_at_line_zero() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "contents": { "kind": "markdown", "value": "docs" },
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 5 }
                }
            }
        });
        let region_start_line = 10;

        let transformed = transform_hover_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let hover = transformed.unwrap();
        let range = hover.range.unwrap();
        assert_eq!(
            range.start.line, 10,
            "Virtual line 0 should map to region_start_line"
        );
        assert_eq!(
            range.end.line, 10,
            "Virtual line 0 should map to region_start_line"
        );
    }

    #[test]
    fn hover_response_with_no_result_key_returns_none() {
        // JSON-RPC error response has no "result" key
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });

        let transformed = transform_hover_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn hover_response_with_malformed_result_returns_none() {
        // Result is a string instead of a Hover object
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_a_hover_object"
        });

        let transformed = transform_hover_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn hover_response_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "contents": { "kind": "markdown", "value": "docs" },
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                }
            }
        });
        let region_start_line = 10;

        let transformed = transform_hover_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let hover = transformed.unwrap();
        let range = hover.range.unwrap();
        assert_eq!(
            range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
        assert_eq!(
            range.end.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }
}
