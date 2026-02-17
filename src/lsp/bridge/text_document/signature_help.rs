//! Signature help request handling for bridge connections.
//!
//! This module provides signature help request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{Position, SignatureHelp};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_position_based_request};

/// Build a JSON-RPC signature help request for a downstream language server.
fn build_signature_help_request(
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
        "textDocument/signatureHelp",
    )
}

/// Transform a signature help response from virtual to host document coordinates.
///
/// SignatureHelp responses contain activeSignature and activeParameter indices,
/// not coordinates, so no transformation is needed. This function extracts the
/// SignatureHelp from the JSON-RPC response and returns it typed.
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `_region_start_line` - The starting line (unused, kept for API consistency)
///
/// # Returns
/// * `Some(SignatureHelp)` if the response contains valid signature help data
/// * `None` if the result is null or cannot be parsed
fn transform_signature_help_response_to_host(
    mut response: serde_json::Value,
    _region_start_line: u32,
) -> Option<SignatureHelp> {
    // SignatureHelp doesn't have ranges that need transformation.
    // activeSignature and activeParameter are indices, not coordinates.
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/signatureHelp: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse the result into a SignatureHelp struct
    serde_json::from_value(result).ok()
}

impl LanguageServerPool {
    /// Send a signature help request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing signature-help-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_signature_help_request(
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
    ) -> io::Result<Option<SignatureHelp>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/signatureHelp") {
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
                build_signature_help_request(
                    virtual_uri,
                    host_position,
                    region_start_line,
                    request_id,
                )
            },
            |response, _ctx| transform_signature_help_response_to_host(response, region_start_line),
        )
        .await
    }
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
    // SignatureHelp request tests
    // ==========================================================================

    #[test]
    fn signature_help_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_signature_help_request(&virtual_uri, test_position(), 3, test_request_id());

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn signature_help_request_translates_position_to_virtual_coordinates() {
        // Host line 5, region starts at line 3 -> virtual line 2
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request =
            build_signature_help_request(&virtual_uri, test_position(), 3, test_request_id());

        assert_position_request(&request, "textDocument/signatureHelp", 2);
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
        let request = build_signature_help_request(
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
    // SignatureHelp response transformation tests
    // ==========================================================================

    #[test]
    fn signature_help_response_extracts_typed_data() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "signatures": [
                    {
                        "label": "function(arg1: string, arg2: number)",
                        "parameters": [
                            { "label": "arg1: string" },
                            { "label": "arg2: number" }
                        ]
                    }
                ],
                "activeSignature": 0,
                "activeParameter": 1
            }
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let signature_help = transformed.unwrap();
        assert_eq!(signature_help.signatures.len(), 1);
        assert_eq!(
            signature_help.signatures[0].label,
            "function(arg1: string, arg2: number)"
        );
        assert_eq!(signature_help.active_signature, Some(0));
        assert_eq!(signature_help.active_parameter, Some(1));
    }

    #[test]
    fn signature_help_response_null_result_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": null
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(transformed.is_none(), "Null result should return None");
    }

    #[test]
    fn signature_help_response_missing_result_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(
            transformed.is_none(),
            "Missing result field should return None"
        );
    }

    #[test]
    fn signature_help_response_invalid_format_returns_none() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "invalid": "format"
            }
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(
            transformed.is_none(),
            "Invalid format should return None rather than panic"
        );
    }

    #[test]
    fn signature_help_response_with_malformed_result_returns_none() {
        // Result is a string instead of a SignatureHelp object
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_a_signature_help_object"
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(transformed.is_none(), "Malformed result should return None");
    }

    #[test]
    fn signature_help_response_error_response_returns_none() {
        // JSON-RPC error response has no "result" key
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(transformed.is_none(), "Error response should return None");
    }

    #[test]
    fn signature_help_response_with_minimal_data() {
        // SignatureHelp with only required fields
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "signatures": [
                    {
                        "label": "print(...)"
                    }
                ]
            }
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let signature_help = transformed.unwrap();
        assert_eq!(signature_help.signatures.len(), 1);
        assert_eq!(signature_help.signatures[0].label, "print(...)");
        assert_eq!(signature_help.active_signature, None);
        assert_eq!(signature_help.active_parameter, None);
    }

    #[test]
    fn signature_help_response_with_multiple_signatures() {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "signatures": [
                    {
                        "label": "overload1(x: number)"
                    },
                    {
                        "label": "overload2(x: string)"
                    },
                    {
                        "label": "overload3(x: boolean)"
                    }
                ],
                "activeSignature": 1
            }
        });
        let region_start_line = 3;

        let transformed = transform_signature_help_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let signature_help = transformed.unwrap();
        assert_eq!(signature_help.signatures.len(), 3);
        assert_eq!(signature_help.signatures[0].label, "overload1(x: number)");
        assert_eq!(signature_help.signatures[1].label, "overload2(x: string)");
        assert_eq!(signature_help.signatures[2].label, "overload3(x: boolean)");
        assert_eq!(signature_help.active_signature, Some(1));
    }
}
