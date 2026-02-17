//! Moniker request handling for bridge connections.
//!
//! This module provides moniker request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{Moniker, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_position_based_request};

impl LanguageServerPool {
    /// Send a moniker request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request`](Self::execute_bridge_request) for the
    /// full lifecycle, providing moniker-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_moniker_request(
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
    ) -> io::Result<Option<Vec<Moniker>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/moniker") {
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
                build_moniker_request(virtual_uri, host_position, region_start_line, request_id)
            },
            |response, _ctx| transform_moniker_response_to_host(response),
        )
        .await
    }
}

/// Build a JSON-RPC moniker request for a downstream language server.
fn build_moniker_request(
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
        "textDocument/moniker",
    )
}

/// Transform a moniker response from the downstream language server.
///
/// Moniker responses are arrays of items with scheme, identifier, unique, and kind.
/// These are non-coordinate data, so no line transformation is needed.
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
fn transform_moniker_response_to_host(mut response: serde_json::Value) -> Option<Vec<Moniker>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/moniker: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse into typed Vec<Moniker>
    // Moniker doesn't have ranges that need transformation.
    // scheme, identifier, unique, and kind are all non-coordinate data.
    serde_json::from_value(result).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::VirtualDocumentUri;
    use serde_json::json;

    #[test]
    fn moniker_request_uses_virtual_uri() {
        use tower_lsp_server::ls_types::Uri;
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let position = Position {
            line: 5,
            character: 10,
        };
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_moniker_request(&virtual_uri, position, 3, RequestId::new(42));

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
    fn moniker_request_translates_position_to_virtual_coordinates() {
        use tower_lsp_server::ls_types::Uri;
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
        let request = build_moniker_request(&virtual_uri, position, 3, RequestId::new(42));

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 42);
        assert_eq!(request["method"], "textDocument/moniker");
        assert_eq!(request["params"]["position"]["line"], 2);
        assert_eq!(request["params"]["position"]["character"], 10);
    }

    #[test]
    fn moniker_response_returns_typed_vec() {
        // Moniker[] has scheme/identifier/unique/kind - no position/range data
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "scheme": "tsc",
                    "identifier": "typescript:foo:bar:Baz",
                    "unique": "document",
                    "kind": "export"
                },
                {
                    "scheme": "npm",
                    "identifier": "package:module:Class.method",
                    "unique": "scheme",
                    "kind": "local"
                }
            ]
        });

        let transformed = transform_moniker_response_to_host(response);

        assert!(transformed.is_some());
        let monikers = transformed.unwrap();
        assert_eq!(monikers.len(), 2);
        assert_eq!(monikers[0].scheme, "tsc");
        assert_eq!(monikers[0].identifier, "typescript:foo:bar:Baz");
        assert_eq!(monikers[1].scheme, "npm");
        assert_eq!(monikers[1].identifier, "package:module:Class.method");
    }

    #[test]
    fn moniker_response_with_null_result_returns_none() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": null });

        let transformed = transform_moniker_response_to_host(response);
        assert!(transformed.is_none());
    }

    #[test]
    fn moniker_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed = transform_moniker_response_to_host(response);
        assert!(transformed.is_some());
        let monikers = transformed.unwrap();
        assert!(monikers.is_empty());
    }

    #[test]
    fn moniker_response_with_no_result_key_returns_none() {
        // JSON-RPC error response has no "result" key
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "error": { "code": -32600, "message": "Invalid Request" }
        });

        let transformed = transform_moniker_response_to_host(response);
        assert!(transformed.is_none());
    }

    #[test]
    fn moniker_response_with_malformed_result_returns_none() {
        // Result is a string instead of a Moniker array
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": "not_an_array"
        });

        let transformed = transform_moniker_response_to_host(response);
        assert!(transformed.is_none());
    }
}
