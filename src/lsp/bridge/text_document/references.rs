//! References request handling for bridge connections.
//!
//! This module provides references request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{Location, Position, Uri};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{build_position_based_request, transform_location_for_goto};

impl LanguageServerPool {
    /// Send a references request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing references-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_references_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        include_declaration: bool,
        upstream_request_id: UpstreamId,
    ) -> io::Result<Option<Vec<Location>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/references") {
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
                let mut request = build_position_based_request(
                    virtual_uri,
                    host_position,
                    region_start_line,
                    request_id,
                    "textDocument/references",
                );
                // Add the context parameter required by references request
                if let Some(params) = request.get_mut("params") {
                    params["context"] = serde_json::json!({
                        "includeDeclaration": include_declaration
                    });
                }
                request
            },
            |response, ctx| {
                transform_references_response_to_host(
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

/// Transform references response to typed Vec<Location> format.
///
/// This function handles the references endpoint response which returns
/// Location[] | null according to the LSP spec.
///
/// # URI Filtering Logic
///
/// Same as goto endpoints:
/// - Real file URIs → keep as-is (cross-file jumps)
/// - Same virtual URI as request → transform coordinates
/// - Different virtual URI → filter out (cross-region, can't transform safely)
///
/// Empty arrays after filtering are preserved to distinguish "searched, found nothing"
/// from "search failed" (None).
///
/// # Arguments
/// * `response` - Raw JSON-RPC response envelope (`{"result": {...}}`)
/// * `request_virtual_uri` - The virtual URI from the request
/// * `host_uri` - The pre-parsed host URI to use in transformed responses
/// * `region_start_line` - Line offset to add when transforming to host coordinates
fn transform_references_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    region_start_line: u32,
) -> Option<Vec<Location>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/references: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    // The LSP spec defines ReferenceResponse as: Location[] | null
    // References only returns arrays of Location (simpler than goto endpoints)

    if result.is_array() {
        let arr = result.as_array()?;
        if arr.is_empty() {
            // Preserve empty arrays (semantic: "searched, found nothing")
            return Some(vec![]);
        }

        // Location[] → transform each location
        if let Ok(locations) = serde_json::from_value::<Vec<Location>>(result) {
            let transformed: Vec<Location> = locations
                .into_iter()
                .filter_map(|location| {
                    transform_location_for_goto(
                        location,
                        request_virtual_uri,
                        host_uri,
                        region_start_line,
                    )
                })
                .collect();

            // Preserve empty array after filtering
            return Some(transformed);
        }
    }

    // Failed to deserialize as Location[]
    None
}

#[cfg(test)]
mod tests {
    use super::transform_references_response_to_host;

    /// Standard test host URI used across most tests.
    fn test_host_uri() -> tower_lsp_server::ls_types::Uri {
        let url = url::Url::parse("file:///project/doc.md").unwrap();
        crate::lsp::lsp_impl::url_to_uri(&url).expect("test URL should convert to URI")
    }

    // ==========================================================================
    // References response transformation tests
    // ==========================================================================

    #[test]
    fn references_response_with_null_result_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": null
        });

        let transformed = transform_references_response_to_host(
            response,
            "file:///virtual.lua",
            &test_host_uri(),
            5,
        );

        assert!(transformed.is_none());
    }

    #[test]
    fn references_response_with_empty_array_preserves_empty() {
        // Server explicitly returns [] - preserve to distinguish from null
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": []
        });

        let transformed = transform_references_response_to_host(
            response,
            "file:///project/kakehashi-virtual-uri-region-0.lua",
            &test_host_uri(),
            5,
        );

        assert!(transformed.is_some());
        let locations = transformed.unwrap();
        assert!(
            locations.is_empty(),
            "Should preserve empty array from server"
        );
    }

    #[test]
    fn references_response_transforms_single_location_with_same_virtual_uri() {
        let virtual_uri = "file:///project/kakehashi-virtual-uri-region-0.lua";
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "uri": virtual_uri,
                    "range": {
                        "start": { "line": 2, "character": 4 },
                        "end": { "line": 2, "character": 9 }
                    }
                }
            ]
        });
        let host_uri = test_host_uri();
        let region_start_line = 10;

        let transformed = transform_references_response_to_host(
            response,
            virtual_uri,
            &host_uri,
            region_start_line,
        );

        assert!(transformed.is_some());
        let locations = transformed.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri, host_uri);
        assert_eq!(locations[0].range.start.line, 12); // 2 + 10
        assert_eq!(locations[0].range.end.line, 12);
        assert_eq!(locations[0].range.start.character, 4);
        assert_eq!(locations[0].range.end.character, 9);
    }

    #[test]
    fn references_response_preserves_real_file_uris() {
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

        let transformed = transform_references_response_to_host(
            response,
            virtual_uri,
            &host_uri,
            region_start_line,
        );

        assert!(transformed.is_some());
        let locations = transformed.unwrap();
        assert_eq!(locations.len(), 1);
        assert_eq!(locations[0].uri.as_str(), real_file_uri);
        assert_eq!(locations[0].range.start.line, 10); // Unchanged
    }

    #[test]
    fn references_response_filters_cross_region_virtual_uris() {
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

        let transformed = transform_references_response_to_host(
            response,
            request_virtual_uri,
            &host_uri,
            region_start_line,
        );

        // Should filter out cross-region virtual URI, resulting in empty array
        assert!(transformed.is_some());
        let locations = transformed.unwrap();
        assert!(
            locations.is_empty(),
            "Should have empty array after filtering"
        );
    }

    #[test]
    fn references_response_filters_mixed_with_cross_region() {
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

        let transformed = transform_references_response_to_host(
            response,
            request_virtual_uri,
            &host_uri,
            region_start_line,
        );

        assert!(transformed.is_some());
        let locations = transformed.unwrap();
        assert_eq!(locations.len(), 2); // Cross-region filtered out
        assert_eq!(locations[0].uri, host_uri);
        assert_eq!(locations[0].range.start.line, 3); // Transformed: 0 + 3
        assert_eq!(locations[1].uri.as_str(), real_file_uri);
        assert_eq!(locations[1].range.start.line, 10); // Preserved
    }
}
