//! PrepareRename request handling for bridge connections.
//!
//! This module provides prepareRename request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{Position, PrepareRenameResponse, TextDocumentPositionParams};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, build_position_based_request,
};

impl LanguageServerPool {
    /// Send a prepareRename request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing prepareRename-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_prepare_rename_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        host_position: Position,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<PrepareRenameResponse>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/prepareRename") {
            // Returning None would cause clients to treat the symbol as "not renameable",
            // silently disabling rename for servers that only advertise textDocument/rename.
            if handle.has_capability("textDocument/rename") {
                return Ok(Some(PrepareRenameResponse::DefaultBehavior {
                    default_behavior: true,
                }));
            }
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_prepare_rename_request(virtual_uri, host_position, &offset, request_id)
            },
            |response, ctx| transform_prepare_rename_response_to_host(response, ctx.offset),
        )
        .await
    }
}

/// Build a JSON-RPC prepareRename request for a downstream language server.
///
/// PrepareRename is a simple position-based request with no additional parameters
/// beyond the text document position.
fn build_prepare_rename_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
) -> JsonRpcRequest<TextDocumentPositionParams> {
    build_position_based_request(
        virtual_uri,
        host_position,
        offset,
        request_id,
        "textDocument/prepareRename",
    )
}

/// Parse a JSON-RPC prepareRename response and transform coordinates to host document space.
///
/// The response can be one of three variants per LSP spec:
/// 1. `Range` - just the range of the symbol
/// 2. `{ range, placeholder }` - range plus suggested name
/// 3. `{ defaultBehavior: true }` - use client default behavior
///
/// For variants 1 and 2, the range must be translated from virtual to host coordinates.
/// Variant 3 has no coordinates to transform.
fn transform_prepare_rename_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
) -> Option<PrepareRenameResponse> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/prepareRename: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    let mut prepare_rename: PrepareRenameResponse = serde_json::from_value(result).ok()?;

    // Transform range coordinates for variants that contain a Range
    match &mut prepare_rename {
        PrepareRenameResponse::Range(range) => {
            translate_virtual_range_to_host(range, offset);
        }
        PrepareRenameResponse::RangeWithPlaceholder { range, .. } => {
            translate_virtual_range_to_host(range, offset);
        }
        PrepareRenameResponse::DefaultBehavior { .. } => {
            // No coordinates to transform
        }
    }

    Some(prepare_rename)
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::*;
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    // ==========================================================================
    // Request builder tests
    // ==========================================================================

    #[test]
    fn prepare_rename_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_prepare_rename_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn prepare_rename_request_translates_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_prepare_rename_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_position_request(&request, "textDocument/prepareRename", 2);
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[test]
    fn prepare_rename_response_range_transforms_to_host() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "start": { "line": 2, "character": 6 },
                "end": { "line": 2, "character": 11 }
            }
        });

        let result = transform_prepare_rename_response_to_host(response, &RegionOffset::new(10, 0));

        let result = result.expect("Should parse Range variant");
        match result {
            PrepareRenameResponse::Range(range) => {
                assert_eq!(range.start.line, 12); // 2 + 10
                assert_eq!(range.end.line, 12);
                assert_eq!(range.start.character, 6);
                assert_eq!(range.end.character, 11);
            }
            _ => panic!("Expected Range variant"),
        }
    }

    #[test]
    fn prepare_rename_response_range_with_placeholder_transforms_to_host() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "range": {
                    "start": { "line": 0, "character": 6 },
                    "end": { "line": 0, "character": 7 }
                },
                "placeholder": "x"
            }
        });

        let result = transform_prepare_rename_response_to_host(response, &RegionOffset::new(5, 0));

        let result = result.expect("Should parse RangeWithPlaceholder variant");
        match result {
            PrepareRenameResponse::RangeWithPlaceholder { range, placeholder } => {
                assert_eq!(range.start.line, 5); // 0 + 5
                assert_eq!(range.end.line, 5);
                assert_eq!(range.start.character, 6);
                assert_eq!(range.end.character, 7);
                assert_eq!(placeholder, "x");
            }
            _ => panic!("Expected RangeWithPlaceholder variant"),
        }
    }

    #[test]
    fn prepare_rename_response_default_behavior_passes_through() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "defaultBehavior": true
            }
        });

        let result = transform_prepare_rename_response_to_host(response, &RegionOffset::new(5, 0));

        let result = result.expect("Should parse DefaultBehavior variant");
        match result {
            PrepareRenameResponse::DefaultBehavior { default_behavior } => {
                assert!(default_behavior);
            }
            _ => panic!("Expected DefaultBehavior variant"),
        }
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42}))]
    fn prepare_rename_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        let result = transform_prepare_rename_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(result.is_none());
    }

    #[test]
    fn prepare_rename_response_range_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "start": { "line": u32::MAX, "character": 0 },
                "end": { "line": u32::MAX, "character": 5 }
            }
        });

        let result = transform_prepare_rename_response_to_host(response, &RegionOffset::new(10, 0));

        let result = result.expect("Should parse Range variant");
        match result {
            PrepareRenameResponse::Range(range) => {
                assert_eq!(range.start.line, u32::MAX, "Should saturate, not panic");
                assert_eq!(range.end.line, u32::MAX, "Should saturate, not panic");
            }
            _ => panic!("Expected Range variant"),
        }
    }
}
