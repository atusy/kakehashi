//! Code lens request handling for bridge connections, with host/virtual
//! coordinate transformation.
//!
//! Like document link requests, code lens requests operate on the entire
//! document — they take no position parameter.
//!
//! kakehashi does not advertise `codeLens.resolveProvider`, so lenses that rely
//! on lazy resolution (no `command`, only `data`) are dropped: per the LSP spec
//! a lens without a command is unresolved, and without resolve support the
//! client could never materialize it.
//!
//! Requests are queued via the channel-based writer task (`send_request()`) for
//! FIFO ordering with other messages (ls-bridge-message-ordering single-writer loop).

use std::io;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::CodeLens;
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    DocumentParams, JsonRpcRequest, RegionOffset, RequestId, VirtualDocumentUri,
    build_whole_document_request, response_has_jsonrpc_error,
};

impl LanguageServerPool {
    /// Send a code lens request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing code-lens-specific request building and response
    /// transformation.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_code_lens_request(
        &self,
        server_name: &str,
        server_config: &BridgeServerConfig,
        host_uri: &Url,
        injection_language: &str,
        region_id: &str,
        offset: RegionOffset,
        virtual_content: &str,
        upstream_request_id: Option<UpstreamId>,
    ) -> io::Result<Option<Vec<CodeLens>>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/codeLens") {
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
            build_code_lens_request,
            |response, ctx| transform_code_lens_response_to_host(response, ctx.offset),
        )
        .await
    }
}

/// Build a JSON-RPC code lens request for a downstream language server.
fn build_code_lens_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
) -> JsonRpcRequest<DocumentParams> {
    build_whole_document_request(virtual_uri, request_id, "textDocument/codeLens")
}

/// Transform a code lens response from virtual to host document coordinates.
///
/// Each lens's `range` is translated by `offset`. Unresolved lenses (no
/// `command`) are dropped because kakehashi does not support `codeLens/resolve`
/// yet — forwarding them would leave the client with lenses it can never
/// resolve.
fn transform_code_lens_response_to_host(
    mut response: serde_json::Value,
    offset: &RegionOffset,
) -> Option<Vec<CodeLens>> {
    if response_has_jsonrpc_error(&response, "textDocument/codeLens") {
        return None;
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;

    if result.is_null() {
        return None;
    }

    let mut lenses: Vec<CodeLens> = serde_json::from_value(result).ok()?;

    lenses.retain(|lens| lens.command.is_some());
    for lens in &mut lenses {
        translate_virtual_range_to_host(&mut lens.range, offset);
    }

    Some(lenses)
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
    fn code_lens_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_code_lens_request(&virtual_uri, RequestId::new(42));

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn code_lens_request_has_correct_method_and_no_position() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_code_lens_request(&virtual_uri, RequestId::new(123));

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 123);
        assert_eq!(json["method"], "textDocument/codeLens");
        assert!(
            json["params"].get("position").is_none(),
            "CodeLens request should not have position parameter"
        );
    }

    // ==========================================================================
    // Response transformation tests
    // ==========================================================================

    #[test]
    fn code_lens_response_transforms_ranges_to_host_coordinates() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 10 }
                    },
                    "command": {
                        "title": "3 references",
                        "command": "editor.action.showReferences"
                    }
                }
            ]
        });

        let transformed = transform_code_lens_response_to_host(response, &RegionOffset::new(5, 0));

        let lenses = transformed.expect("Should parse code lenses");
        assert_eq!(lenses.len(), 1);
        assert_eq!(lenses[0].range.start.line, 5);
        assert_eq!(lenses[0].range.end.line, 5);
        assert_eq!(
            lenses[0].command.as_ref().map(|c| c.title.as_str()),
            Some("3 references")
        );
    }

    #[test]
    fn code_lens_response_drops_unresolved_lenses_without_command() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [
                {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 5 }
                    },
                    "data": { "kind": "references" }
                },
                {
                    "range": {
                        "start": { "line": 1, "character": 0 },
                        "end": { "line": 1, "character": 5 }
                    },
                    "command": { "title": "Run test", "command": "test.run" }
                }
            ]
        });

        let transformed = transform_code_lens_response_to_host(response, &RegionOffset::new(3, 0));

        let lenses = transformed.expect("Should parse code lenses");
        assert_eq!(
            lenses.len(),
            1,
            "Unresolved lens (data only, no command) should be dropped"
        );
        assert_eq!(lenses[0].range.start.line, 4);
        assert_eq!(
            lenses[0].command.as_ref().map(|c| c.title.as_str()),
            Some("Run test")
        );
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::no_result_key(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_an_array"}))]
    fn code_lens_response_returns_none_for_invalid(#[case] response: serde_json::Value) {
        let transformed = transform_code_lens_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(transformed.is_none());
    }

    #[test]
    fn code_lens_response_with_empty_array_returns_empty_vec() {
        let response = json!({ "jsonrpc": "2.0", "id": 42, "result": [] });

        let transformed = transform_code_lens_response_to_host(response, &RegionOffset::new(5, 0));
        assert!(transformed.expect("Should parse empty array").is_empty());
    }

    #[test]
    fn code_lens_response_saturates_on_overflow() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "range": {
                    "start": { "line": u32::MAX, "character": 0 },
                    "end": { "line": u32::MAX, "character": 5 }
                },
                "command": { "title": "x", "command": "y" }
            }]
        });

        let transformed = transform_code_lens_response_to_host(response, &RegionOffset::new(10, 0));

        let lenses = transformed.expect("Should parse code lenses");
        assert_eq!(
            lenses[0].range.start.line,
            u32::MAX,
            "Overflow should saturate at u32::MAX, not panic"
        );
    }
}
