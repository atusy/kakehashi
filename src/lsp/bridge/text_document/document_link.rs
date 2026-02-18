//! Document link request handling for bridge connections.
//!
//! This module provides document link request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! Unlike position-based requests (hover, definition, etc.), document link requests
//! operate on the entire document - they don't take a position parameter.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::DocumentLink;
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{RequestId, VirtualDocumentUri, build_whole_document_request};

impl LanguageServerPool {
    /// Send a document link request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing document-link-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_document_link_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        region_start_line: u32,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<DocumentLink>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/documentLink") {
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
            build_document_link_request,
            |response, ctx| {
                transform_document_link_response_to_host(response, ctx.region_start_line)
            },
        )
        .await
    }
}

/// Build a JSON-RPC document link request for a downstream language server.
///
/// Unlike position-based requests (hover, definition, etc.), DocumentLinkParams
/// only has a textDocument field - no position. The request asks for all links
/// in the entire document.
fn build_document_link_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> serde_json::Value {
    build_whole_document_request(virtual_uri, request_id, "textDocument/documentLink")
}

/// Transform a document link response from virtual to host document coordinates.
///
/// DocumentLink responses are arrays of items with range, target, tooltip, and data fields.
/// Only the range needs transformation - target, tooltip, and data are preserved unchanged.
///
/// # Arguments
/// * `response` - The JSON-RPC response from the downstream language server
/// * `region_start_line` - The starting line of the injection region in the host document
fn transform_document_link_response_to_host(
    mut response: serde_json::Value,
    region_start_line: u32,
) -> Option<Vec<DocumentLink>> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/documentLink: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    // Parse into typed Vec<DocumentLink>
    let mut links: Vec<DocumentLink> = serde_json::from_value(result).ok()?;

    // Transform ranges to host coordinates
    for link in &mut links {
        link.range.start.line = link.range.start.line.saturating_add(region_start_line);
        link.range.end.line = link.range.end.line.saturating_add(region_start_line);
    }

    Some(links)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::bridge::protocol::VirtualDocumentUri;
    use rstest::rstest;
    use serde_json::json;

    // ==========================================================================
    // Document link request tests
    // ==========================================================================

    #[test]
    fn document_link_request_uses_virtual_uri() {
        use tower_lsp_server::ls_types::Uri;
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_document_link_request(&virtual_uri, RequestId::new(42));

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
    fn document_link_request_has_correct_method_and_no_position() {
        use tower_lsp_server::ls_types::Uri;
        use url::Url;

        let host_uri: Uri =
            crate::lsp::lsp_impl::url_to_uri(&Url::parse("file:///project/doc.md").unwrap())
                .unwrap();
        let virtual_uri = VirtualDocumentUri::new(&host_uri, "lua", "region-0");
        let request = build_document_link_request(&virtual_uri, RequestId::new(123));

        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(request["id"], 123);
        assert_eq!(request["method"], "textDocument/documentLink");
        assert!(
            request["params"].get("position").is_none(),
            "DocumentLink request should not have position parameter"
        );
    }

    // ==========================================================================
    // Document link response transformation tests
    // ==========================================================================

    #[test]
    fn document_link_response_transforms_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 10 },
                        "end": { "line": 0, "character": 25 }
                    },
                    "target": "file:///some/module.lua"
                },
                {
                    "range": {
                        "start": { "line": 2, "character": 5 },
                        "end": { "line": 2, "character": 15 }
                    }
                }
            ]
        });
        let region_start_line = 5;

        let transformed = transform_document_link_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].range.start.line, 5);
        assert_eq!(links[0].range.end.line, 5);
        assert_eq!(links[0].range.start.character, 10);
        assert_eq!(
            links[0].target.as_ref().map(|u| u.as_str()),
            Some("file:///some/module.lua")
        );
        assert_eq!(links[1].range.start.line, 7);
        assert_eq!(links[1].range.end.line, 7);
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn document_link_response_returns_none_for_invalid_response(
        #[case] response: serde_json::Value,
    ) {
        let transformed = transform_document_link_response_to_host(response, 5);
        assert!(transformed.is_none());
    }

    #[test]
    fn document_link_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed = transform_document_link_response_to_host(response, 5);
        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert!(links.is_empty());
    }

    #[test]
    fn document_link_response_preserves_target_and_tooltip() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 0, "character": 0 },
                    "end": { "line": 0, "character": 10 }
                },
                "target": "file:///target.lua",
                "tooltip": "Go to definition"
            }]
        });
        let region_start_line = 3;

        let transformed = transform_document_link_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links[0].range.start.line, 3);
        assert_eq!(
            links[0].target.as_ref().map(|u| u.as_str()),
            Some("file:///target.lua")
        );
        assert_eq!(links[0].tooltip.as_deref(), Some("Go to definition"));
    }

    #[test]
    fn document_link_response_without_target_transforms_range() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": 1, "character": 5 },
                    "end": { "line": 1, "character": 20 }
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_document_link_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(links[0].range.start.line, 11);
        assert_eq!(links[0].range.end.line, 11);
        assert!(links[0].target.is_none());
    }

    #[test]
    fn document_link_response_transformation_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_document_link_response_to_host(response, region_start_line);

        assert!(transformed.is_some());
        let links = transformed.unwrap();
        assert_eq!(
            links[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }
}
