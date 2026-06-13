//! LinkedEditingRange request handling for bridge connections.
//!
//! This module provides linkedEditingRange request functionality for downstream
//! language servers, handling the coordinate transformation between host and
//! virtual documents.
//!
//! # Single-Writer Loop (ls-bridge-message-ordering)
//!
//! This handler uses `send_request()` to queue requests via the channel-based
//! writer task, ensuring FIFO ordering with other messages.

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{LinkedEditingRanges, Position, TextDocumentPositionParams};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri, build_position_based_request,
    response_has_jsonrpc_error,
};

impl LanguageServerPool {
    /// Send a linkedEditingRange request and wait for the response.
    ///
    /// Delegates to [`execute_position_bridge_request_with_handle`](Self::execute_position_bridge_request_with_handle)
    /// for the full lifecycle, providing linkedEditingRange-specific request
    /// building and response transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_linked_editing_range_request(
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
    ) -> io::Result<Option<LinkedEditingRanges>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/linkedEditingRange") {
            return Ok(None);
        }
        self.execute_position_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            host_position,
            "textDocument/linkedEditingRange",
            |virtual_uri, request_id| {
                build_linked_editing_range_request(virtual_uri, host_position, &offset, request_id)
            },
            |response, ctx| transform_linked_editing_range_response_to_host(response, ctx.offset),
        )
        .await
    }
}

/// Build a JSON-RPC linkedEditingRange request for a downstream language server.
///
/// LinkedEditingRange is a simple position-based request with no additional
/// parameters beyond the text document position.
fn build_linked_editing_range_request(
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
        "textDocument/linkedEditingRange",
    )
}

/// Parse a JSON-RPC linkedEditingRange response and transform coordinates to
/// host document space.
///
/// All ranges describe occurrences of the same text inside the injection
/// region, so each one is translated by the region offset. `wordPattern`
/// carries no coordinates and passes through unchanged.
///
/// An empty `ranges` list normalizes to `None`: the preferred fan-in treats
/// any `Some` as a winning result, so passing it through would let one
/// server's "nothing here" mask another server's actual ranges and would
/// surface an empty list instead of `null` upstream.
fn transform_linked_editing_range_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
) -> Option<LinkedEditingRanges> {
    if response_has_jsonrpc_error(&response, "textDocument/linkedEditingRange") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    let mut linked: LinkedEditingRanges = serde_json::from_value(result).ok()?;
    if linked.ranges.is_empty() {
        return None;
    }

    for range in &mut linked.ranges {
        translate_virtual_range_to_host(range, offset);
    }

    Some(linked)
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
    fn linked_editing_range_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_linked_editing_range_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn linked_editing_range_request_translates_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_linked_editing_range_request(
            &virtual_uri,
            test_position(),
            &RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_position_request(&request, "textDocument/linkedEditingRange", 2);
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[test]
    fn linked_editing_range_response_transforms_ranges_to_host() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "ranges": [
                    {
                        "start": { "line": 0, "character": 4 },
                        "end": { "line": 0, "character": 9 }
                    },
                    {
                        "start": { "line": 2, "character": 6 },
                        "end": { "line": 2, "character": 11 }
                    }
                ]
            }
        });

        let result =
            transform_linked_editing_range_response_to_host(response, &RegionOffset::new(10, 0));

        let linked = result.expect("Should parse LinkedEditingRanges");
        assert_eq!(linked.ranges.len(), 2);
        assert_eq!(linked.ranges[0].start.line, 10);
        assert_eq!(linked.ranges[0].end.line, 10);
        assert_eq!(linked.ranges[0].start.character, 4);
        assert_eq!(linked.ranges[1].start.line, 12);
        assert_eq!(linked.ranges[1].end.line, 12);
        assert!(linked.word_pattern.is_none());
    }

    #[test]
    fn linked_editing_range_response_preserves_word_pattern() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "ranges": [
                    {
                        "start": { "line": 1, "character": 0 },
                        "end": { "line": 1, "character": 3 }
                    }
                ],
                "wordPattern": "[a-zA-Z_]+"
            }
        });

        let result =
            transform_linked_editing_range_response_to_host(response, &RegionOffset::new(5, 0));

        let linked = result.expect("Should parse LinkedEditingRanges");
        assert_eq!(linked.ranges[0].start.line, 6);
        assert_eq!(linked.word_pattern.as_deref(), Some("[a-zA-Z_]+"));
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42}))]
    #[case::error_response(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_object"}))]
    #[case::empty_ranges(json!({"jsonrpc": "2.0", "id": 42, "result": {"ranges": []}}))]
    fn linked_editing_range_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        let result =
            transform_linked_editing_range_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(result.is_none());
    }

    #[test]
    fn linked_editing_range_response_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "ranges": [
                    {
                        "start": { "line": u32::MAX, "character": 0 },
                        "end": { "line": u32::MAX, "character": 5 }
                    }
                ]
            }
        });

        let result =
            transform_linked_editing_range_response_to_host(response, &RegionOffset::new(10, 0));

        let linked = result.expect("Should parse LinkedEditingRanges");
        assert_eq!(
            linked.ranges[0].start.line,
            u32::MAX,
            "Should saturate, not panic"
        );
    }
}
