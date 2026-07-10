//! References request handling for bridge connections.
//!
//! This module provides references request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{Location, Position, Uri};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, build_text_document_position_params, response_has_jsonrpc_error,
    transform_location_for_goto,
};
use tower_lsp_server::ls_types::{ReferenceContext, ReferenceParams};

impl LanguageServerPool {
    /// Send a references request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
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
        offset: RegionOffset,
        virtual_content: &str,
        include_declaration: bool,
        upstream_request_id: Option<UpstreamId>,
        client_progress_token: Option<tower_lsp_server::ls_types::NumberOrString>,
    ) -> io::Result<Option<Vec<Location>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/references") {
            return Ok(None);
        }
        self.execute_position_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            host_position,
            "textDocument/references",
            |virtual_uri, request_id| {
                let params = ReferenceParams {
                    text_document_position: build_text_document_position_params(
                        virtual_uri,
                        host_position,
                        &offset,
                    ),
                    context: ReferenceContext {
                        include_declaration,
                    },
                    // Hand the downstream the bridge-minted token so its
                    // `$/progress` routes to this request's aggregator
                    // (ls-bridge-client-progress).
                    work_done_progress_params: tower_lsp_server::ls_types::WorkDoneProgressParams {
                        work_done_token: client_progress_token,
                    },
                    partial_result_params: Default::default(),
                };
                JsonRpcRequest::new(request_id.as_i64(), "textDocument/references", params)
            },
            |response, ctx| {
                transform_references_response_to_host(
                    response,
                    &ctx.virtual_uri_string,
                    ctx.host_uri_lsp,
                    ctx.offset,
                )
            },
        )
        .await?
    }
}

/// Normalize a `references` response (`Location[] | null`) into
/// `io::Result<Option<Vec<Location>>>`, applying the same URI filter as goto:
/// keep real-file URIs, translate matches on the request's virtual URI, drop
/// other virtual URIs (cross-region offsets are unsafe). `Ok(Some(vec))` may
/// be empty — an empty array from the server is preserved as-is, while
/// `Ok(None)` covers the answers the protocol allows that carry no array: a
/// JSON-RPC error response or a `null` result. Protocol violations — a
/// response with neither `result` nor `error`, or a result that is not
/// `Location[] | null` — return `Err`, letting callers surface a warning.
fn transform_references_response_to_host(
    mut response: serde_json::Value,
    request_virtual_uri: &str,
    host_uri: &Uri,
    offset: &RegionOffset,
) -> io::Result<Option<Vec<Location>>> {
    if response_has_jsonrpc_error(&response, "textDocument/references") {
        return Ok(None);
    }
    let Some(result) = response.get_mut("result").map(serde_json::Value::take) else {
        log::warn!(
            target: "kakehashi::bridge",
            "textDocument/references response carries neither result nor error (protocol violation)"
        );
        return Err(io::Error::other(
            "textDocument/references response carries neither result nor error (protocol violation)",
        ));
    };
    if result.is_null() {
        return Ok(None);
    }

    // The LSP spec defines ReferenceResponse as: Location[] | null
    // References only returns arrays of Location (simpler than goto endpoints)

    if result.is_array() {
        let arr = result
            .as_array()
            .expect("result.is_array() was checked above");
        if arr.is_empty() {
            // Preserve empty arrays (semantic: "searched, found nothing")
            return Ok(Some(vec![]));
        }

        // Location[] → transform each location
        match serde_json::from_value::<Vec<Location>>(result) {
            Ok(locations) => {
                let transformed: Vec<Location> = locations
                    .into_iter()
                    .filter_map(|location| {
                        transform_location_for_goto(location, request_virtual_uri, host_uri, offset)
                    })
                    .collect();

                // Preserve empty array after filtering
                return Ok(Some(transformed));
            }
            Err(err) => {
                log::warn!(
                    target: "kakehashi::bridge",
                    "references response did not match Location[]: {err}"
                );
                return Err(io::Error::other(format!(
                    "malformed textDocument/references result from downstream server: {err}"
                )));
            }
        }
    }

    log::warn!(
        target: "kakehashi::bridge",
        "references response did not match Location[]"
    );
    Err(io::Error::other(
        "malformed textDocument/references result from downstream server",
    ))
}

#[cfg(test)]
mod tests {

    use super::super::test_helpers::test_host_uri;
    use super::RegionOffset;
    use super::transform_references_response_to_host;
    use crate::lsp::bridge::test_logging::captured_warnings_for;

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
            &RegionOffset::new(5, 0),
        );

        assert!(transformed.unwrap().is_none());
    }

    #[test]
    fn references_response_with_jsonrpc_error_returns_ok_none() {
        // A downstream error response is a protocol-legal "no answer": it must
        // stay Ok(None), not become Err (which is reserved for violations).
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32603, "message": "internal error" }
        });

        let transformed = transform_references_response_to_host(
            response,
            "file:///virtual.lua",
            &test_host_uri(),
            &RegionOffset::new(5, 0),
        );

        assert!(transformed.unwrap().is_none());
    }

    #[test]
    fn references_response_warns_on_missing_result_success() {
        let warnings = captured_warnings_for(|| {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 42
            });

            let transformed = transform_references_response_to_host(
                response,
                "file:///project/kakehashi-virtual-uri-region-0.lua",
                &test_host_uri(),
                &RegionOffset::new(5, 0),
            );

            assert!(transformed.is_err());
        });

        assert!(
            warnings.iter().any(|message| {
                message.contains("kakehashi::bridge")
                    && message.contains(
                        "textDocument/references response carries neither result nor error (protocol violation)",
                    )
            }),
            "expected missing-result references warning, got {warnings:?}"
        );
    }

    #[test]
    fn references_response_warns_on_malformed_success_result() {
        let warnings = captured_warnings_for(|| {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 42,
                "result": "not references"
            });

            let transformed = transform_references_response_to_host(
                response,
                "file:///project/kakehashi-virtual-uri-region-0.lua",
                &test_host_uri(),
                &RegionOffset::new(5, 0),
            );

            assert!(transformed.is_err());
        });

        assert!(
            warnings.iter().any(|message| {
                message.contains("kakehashi::bridge")
                    && message.contains("references response did not match Location[]")
            }),
            "expected malformed references warning, got {warnings:?}"
        );
    }

    #[test]
    fn references_response_warns_on_malformed_location_array() {
        let warnings = captured_warnings_for(|| {
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 42,
                "result": [
                    {
                        "uri": "file:///project/referenced.lua"
                    }
                ]
            });

            let transformed = transform_references_response_to_host(
                response,
                "file:///project/kakehashi-virtual-uri-region-0.lua",
                &test_host_uri(),
                &RegionOffset::new(5, 0),
            );

            assert!(transformed.is_err());
        });

        assert!(
            warnings.iter().any(|message| {
                message.contains("kakehashi::bridge")
                    && message.contains("references response did not match Location[]")
            }),
            "expected malformed references array warning, got {warnings:?}"
        );
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
            &RegionOffset::new(5, 0),
        );

        let locations = transformed.unwrap().unwrap();
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
            &RegionOffset::new(region_start_line, 0),
        );

        let locations = transformed.unwrap().unwrap();
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
            &RegionOffset::new(region_start_line, 0),
        );

        let locations = transformed.unwrap().unwrap();
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
            &RegionOffset::new(region_start_line, 0),
        );

        // Should filter out cross-region virtual URI, resulting in empty array
        let locations = transformed.unwrap().unwrap();
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
            &RegionOffset::new(region_start_line, 0),
        );

        let locations = transformed.unwrap().unwrap();
        assert_eq!(locations.len(), 2); // Cross-region filtered out
        assert_eq!(locations[0].uri, host_uri);
        assert_eq!(locations[0].range.start.line, 3); // Transformed: 0 + 3
        assert_eq!(locations[1].uri.as_str(), real_file_uri);
        assert_eq!(locations[1].range.start.line, 10); // Preserved
    }
}
