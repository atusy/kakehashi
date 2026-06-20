//! Folding range request handling for bridge connections, with host/virtual
//! coordinate transformation.
//!
//! Like document link requests, folding range requests operate on the entire
//! document — they take no position parameter.
//!
//! Requests are queued via the channel-based writer task (`send_request()`) for
//! FIFO ordering with other messages (ls-bridge-message-ordering single-writer loop).

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::FoldingRange;
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::{
    DocumentParams, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_whole_document_request, response_has_jsonrpc_error,
};

impl LanguageServerPool {
    /// Send a folding range request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing folding-range-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_folding_range_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<FoldingRange>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config, Some(host_uri))
            .await?;
        if !handle.has_capability("textDocument/foldingRange") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            host_uri,
            injection_language,
            region_id,
            &offset,
            virtual_content,
            upstream_request_id,
            build_folding_range_request,
            |response, ctx| transform_folding_range_response_to_host(response, ctx.offset),
        )
        .await
    }
}

/// Build a JSON-RPC folding range request for a downstream language server.
fn build_folding_range_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentParams> {
    build_whole_document_request(virtual_uri, request_id, "textDocument/foldingRange")
}

/// Transform a folding range response from virtual to host document coordinates.
///
/// `FoldingRange` uses bare line numbers plus optional character offsets rather
/// than a `Range`, so translation mirrors `translate_virtual_position_to_host`:
/// lines shift by the region's start line, and the optional characters shift by
/// the per-line column offset of the virtual line they belong to.
fn transform_folding_range_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
) -> Option<Vec<FoldingRange>> {
    if response_has_jsonrpc_error(&response, "textDocument/foldingRange") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    let mut ranges: Vec<FoldingRange> = serde_json::from_value(result).ok()?;

    for folding in &mut ranges {
        let virtual_start_line = folding.start_line;
        let virtual_end_line = folding.end_line;
        folding.start_line = folding.start_line.saturating_add(offset.line());
        folding.end_line = folding.end_line.saturating_add(offset.line());
        folding.start_character = folding
            .start_character
            .map(|c| c.saturating_add(offset.column_for_line(virtual_start_line)));
        folding.end_character = folding
            .end_character
            .map(|c| c.saturating_add(offset.column_for_line(virtual_end_line)));
    }

    Some(ranges)
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
    fn folding_range_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_folding_range_request(&virtual_uri, RequestId::new(42));

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn folding_range_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_folding_range_request(&virtual_uri, RequestId::new(123));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/foldingRange");
        assert!(
            json["params"].get("position").is_none(),
            "FoldingRange request should not have position parameter"
        );
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[test]
    fn folding_range_response_shifts_lines_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": 0, "endLine": 3, "kind": "region" },
                { "startLine": 5, "endLine": 8 }
            ]
        });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(10, 0));

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].start_line, 10);
        assert_eq!(ranges[0].end_line, 13);
        assert_eq!(
            ranges[0].kind,
            Some(tower_lsp_server::ls_types::FoldingRangeKind::Region)
        );
        assert_eq!(ranges[1].start_line, 15);
        assert_eq!(ranges[1].end_line, 18);
    }

    #[test]
    fn folding_range_response_shifts_characters_with_per_line_columns() {
        // Blockquote-style region: every line is prefixed by "> " (2 columns)
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": 0, "startCharacter": 4, "endLine": 1, "endCharacter": 7 }
            ]
        });

        let transformed = transform_folding_range_response_to_host(
            response,
            &RegionOffset::with_per_line_offsets(3, vec![2, 2]),
        );

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(ranges[0].start_line, 3);
        assert_eq!(ranges[0].start_character, Some(6)); // 4 + 2
        assert_eq!(ranges[0].end_line, 4);
        assert_eq!(ranges[0].end_character, Some(9)); // 7 + 2
    }

    #[test]
    fn folding_range_response_preserves_missing_characters() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": 1, "endLine": 2, "collapsedText": "..." }
            ]
        });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(5, 0));

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(ranges[0].start_line, 6);
        assert_eq!(ranges[0].end_line, 7);
        assert!(ranges[0].start_character.is_none());
        assert!(ranges[0].end_character.is_none());
        assert_eq!(ranges[0].collapsed_text.as_deref(), Some("..."));
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn folding_range_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(transformed.is_none());
    }

    #[test]
    fn folding_range_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(transformed.expect("Should parse empty array").is_empty());
    }

    #[test]
    fn folding_range_response_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                { "startLine": u32::MAX, "endLine": u32::MAX }
            ]
        });

        let transformed =
            transform_folding_range_response_to_host(response, &RegionOffset::new(10, 0));

        let ranges = transformed.expect("Should parse folding ranges");
        assert_eq!(
            ranges[0].start_line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }
}
