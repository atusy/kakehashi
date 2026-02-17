//! Definition request handling for bridge connections.
//!
//! This module provides definition request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{LocationLink, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    RequestId, VirtualDocumentUri, build_position_based_request, transform_goto_response_to_host,
};

impl LanguageServerPool {
    /// Send a definition request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing definition-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_definition_request(
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
    ) -> io::Result<Option<Vec<LocationLink>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/definition") {
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
                build_definition_request(virtual_uri, host_position, region_start_line, request_id)
            },
            |response, ctx| {
                transform_goto_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.region_start_line,
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC definition request for a downstream language server.
fn build_definition_request(
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
        "textDocument/definition",
    )
}

#[cfg(test)]
mod tests {
    use super::super::super::protocol::transform_goto_response_to_host;
    use super::*;
    use tower_lsp_server::ls_types::Position;

    // ==========================================================================
    // Test helpers
    // ==========================================================================

    /// Standard test request ID used across most tests.
    fn test_request_id() -> RequestId {
        RequestId::new(42)
    }

    /// Standard test host URI used across most tests.
    fn test_host_uri() -> tower_lsp_server::ls_types::Uri {
        let url = url::Url::parse("file:///project/doc.md").unwrap();
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
    // Definition request tests
    // ==========================================================================

    #[test]
    fn definition_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_definition_request(&virtual_uri, test_position(), 3, test_request_id());

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn definition_request_translates_position_to_virtual_coordinates() {
        // Host line 5, region starts at line 3 -> virtual line 2
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_definition_request(&virtual_uri, test_position(), 3, test_request_id());

        assert_position_request(&request, "textDocument/definition", 2);
    }

    // ==========================================================================
    // Definition response transformation tests
    // ==========================================================================

    #[test]
    fn definition_response_transforms_single_location() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "uri": virtual_uri,
                "range": {
                    "start": { "line": 0, "character": 9 },
                    "end": { "line": 0, "character": 14 }
                }
            }
        });
        let host_uri = test_host_uri();
        let region_start_line = 3;

        let transformed =
            transform_goto_response_to_host(response, virtual_uri, &host_uri, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri, host_uri);
        assert_eq!(links[0].target_range.start.line, 3);
        assert_eq!(links[0].target_range.end.line, 3);
        assert_eq!(links[0].target_selection_range.start.line, 3);
        assert_eq!(links[0].target_selection_range.end.line, 3);
    }

    #[test]
    fn definition_response_transforms_location_array() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "uri": virtual_uri,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    }
                },
                {
                    "uri": virtual_uri,
                    "range": {
                        "start": { "line": 2, "character": 0 },
                        "end": { "line": 2, "character": 8 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 3;

        let transformed =
            transform_goto_response_to_host(response, virtual_uri, &host_uri, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].target_range.start.line, 3);
        assert_eq!(links[1].target_range.start.line, 5);
    }

    #[test]
    fn definition_response_transforms_location_link_array() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "targetUri": virtual_uri,
                    "targetRange": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 10 }
                    },
                    "targetSelectionRange": {
                        "start": { "line": 0, "character": 4 },
                        "end": { "line": 0, "character": 9 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 3;

        let transformed =
            transform_goto_response_to_host(response, virtual_uri, &host_uri, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri, host_uri);
        assert_eq!(links[0].target_range.start.line, 3);
        assert_eq!(links[0].target_selection_range.start.line, 3);
    }

    #[test]
    fn definition_response_preserves_real_file_uris() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let real_file_uri = "file:///project/real_file.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "uri": real_file_uri,
                    "range": {
                        "start": { "line": 10, "character": 0 },
                        "end": { "line": 10, "character": 5 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 5;

        let transformed =
            transform_goto_response_to_host(response, virtual_uri, &host_uri, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri.as_str(), real_file_uri);
        assert_eq!(links[0].target_range.start.line, 10); // Unchanged
    }

    #[test]
    fn definition_response_filters_cross_region_virtual_uris() {
        let request_virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let other_virtual_uri = "file:///project/kakehashi-virtual-uri-region-1.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "uri": other_virtual_uri,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 5;

        let transformed = transform_goto_response_to_host(
            response,
            request_virtual_uri,
            &host_uri,
            region_start_line,
        );

        // Should filter out cross-region virtual URI, resulting in empty array
        // Preserve empty array to distinguish "found nothing" from "failed/null"
        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert!(links.is_empty(), "Should have empty array after filtering");
    }

    #[test]
    fn definition_response_filters_mixed_with_cross_region() {
        let request_virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let other_virtual_uri = "file:///project/kakehashi-virtual-uri-region-1.lua";
        let real_file_uri = "file:///project/real_file.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "uri": request_virtual_uri,
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    }
                },
                {
                    "uri": other_virtual_uri,
                    "range": {
                        "start": { "line": 5, "character": 0 },
                        "end": { "line": 5, "character": 5 }
                    }
                },
                {
                    "uri": real_file_uri,
                    "range": {
                        "start": { "line": 10, "character": 0 },
                        "end": { "line": 10, "character": 5 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 3;

        let transformed = transform_goto_response_to_host(
            response,
            request_virtual_uri,
            &host_uri,
            region_start_line,
        );

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 2); // Cross-region filtered out
        assert_eq!(links[0].target_uri, host_uri);
        assert_eq!(links[0].target_range.start.line, 3); // Transformed
        assert_eq!(links[1].target_uri.as_str(), real_file_uri);
        assert_eq!(links[1].target_range.start.line, 10); // Preserved
    }

    #[test]
    fn definition_response_with_null_result_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": null
        });

        let transformed =
            transform_goto_response_to_host(response, "file:///virtual.lua", &test_host_uri(), 5);

        assert!(transformed.is_none());
    }

    #[test]
    fn definition_response_with_empty_array_preserves_empty() {
        // Server explicitly returns [] - preserve it to distinguish from null
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": []
        });

        let transformed = transform_goto_response_to_host(
            response,
            "file:///project/kakehashi-virtual-uri-region-0.lua",
            &test_host_uri(),
            5,
        );

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert!(links.is_empty(), "Should preserve empty array from server");
    }

    #[test]
    fn definition_response_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "uri": virtual_uri,
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                }
            }
        });
        let host_uri = test_host_uri();
        let region_start_line = 10;

        let transformed =
            transform_goto_response_to_host(response, virtual_uri, &host_uri, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target_range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }

    #[test]
    fn definition_response_transforms_origin_selection_range_in_location_link() {
        // originSelectionRange is returned in virtual coordinates by the downstream
        // server; it must be translated to host coordinates just like target_range
        // and target_selection_range.
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "originSelectionRange": {
                        "start": { "line": 2, "character": 4 },
                        "end": { "line": 2, "character": 9 }
                    },
                    "targetUri": virtual_uri,
                    "targetRange": {
                        "start": { "line": 5, "character": 0 },
                        "end": { "line": 8, "character": 1 }
                    },
                    "targetSelectionRange": {
                        "start": { "line": 5, "character": 4 },
                        "end": { "line": 5, "character": 9 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 10;

        let transformed =
            transform_goto_response_to_host(response, virtual_uri, &host_uri, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].target_uri, host_uri);
        // target_range: line 5 + 10 = 15
        assert_eq!(links[0].target_range.start.line, 15);
        assert_eq!(links[0].target_range.end.line, 18);
        // target_selection_range: line 5 + 10 = 15
        assert_eq!(links[0].target_selection_range.start.line, 15);
        assert_eq!(links[0].target_selection_range.end.line, 15);
        // origin_selection_range: line 2 + 10 = 12 (the bug: currently NOT transformed)
        let origin = links[0]
            .origin_selection_range
            .expect("origin_selection_range should be present");
        assert_eq!(
            origin.start.line, 12,
            "origin_selection_range start line should be translated from virtual (2) to host (12)"
        );
        assert_eq!(
            origin.end.line, 12,
            "origin_selection_range end line should be translated from virtual (2) to host (12)"
        );
        assert_eq!(origin.start.character, 4);
        assert_eq!(origin.end.character, 9);
    }
}
