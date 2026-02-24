//! Completion request handling for bridge connections.
//!
//! This module provides completion request functionality for downstream language servers,
//! handling the coordinate transformation between host and virtual documents.
//!
//! # Single-Writer Loop (ADR-0015)
//!
//! This handler uses `send_request()` and `send_notification()` to queue messages via
//! the channel-based writer task, ensuring FIFO ordering with other messages.

use std::io;

use log::warn;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::BridgeServerConfig;
use tower_lsp_server::ls_types::{CompletionItem, CompletionList, Position};
use url::Url;

use super::super::pool::{LanguageServerPool, UpstreamId};
use super::super::protocol::translate_virtual_range_to_host;
use super::super::protocol::{
    RegionOffset, RequestId, VirtualDocumentUri, build_position_based_request,
};

impl LanguageServerPool {
    /// Send a completion request and wait for the response.
    ///
    /// Delegates to [`execute_bridge_request_with_handle`](Self::execute_bridge_request_with_handle) for the
    /// full lifecycle, providing completion-specific request building and response
    /// transformation.
    ///
    /// Content synchronization (didChange) is handled by the notification pipeline
    /// in `forward_didchange_to_bridges`, which runs during `did_change()` processing
    /// before any subsequent request is handled.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn send_completion_request(
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
    ) -> io::Result<Option<CompletionList>> {
        let handle = self
            .get_or_create_connection(server_name, server_config)
            .await?;
        if !handle.has_capability("textDocument/completion") {
            return Ok(None);
        }
        self.execute_bridge_request_with_handle(
            handle,
            server_name,
            host_uri,
            injection_language,
            region_id,
            offset,
            virtual_content,
            upstream_request_id,
            |virtual_uri, request_id| {
                build_completion_request(virtual_uri, host_position, offset, request_id)
            },
            |response, ctx| {
                transform_completion_response_to_host(
                    response,
                    ctx.offset,
                    Some(EnvelopeContext {
                        server_name,
                        offset: ctx.offset,
                    }),
                )
            },
        )
        .await
    }
}

/// Build a JSON-RPC completion request for a downstream language server.
fn build_completion_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: tower_lsp_server::ls_types::Position,
    offset: RegionOffset,
    request_id: RequestId,
) -> serde_json::Value {
    build_position_based_request(
        virtual_uri,
        host_position,
        offset,
        request_id,
        "textDocument/completion",
    )
}

/// Parse a JSON-RPC completion response and transform coordinates to host document space.
///
/// Normalizes all responses to `CompletionList` format. If the server returns an array,
/// it's wrapped as `CompletionList { isIncomplete: false, items }`.
///
/// Returns `None` for: null results, missing results, and deserialization failures.
///
/// # Arguments
/// * `response` - Raw JSON-RPC response envelope (`{"result": {...}}`)
/// * `offset` - The region offset for coordinate translation
/// * `envelope_ctx` - If `Some`, each item's `data` is wrapped in a routing envelope
fn transform_completion_response_to_host(
    mut response: serde_json::Value,
    offset: RegionOffset,
    envelope_ctx: Option<EnvelopeContext<'_>>,
) -> Option<CompletionList> {
    if let Some(error) = response.get("error") {
        warn!(target: "kakehashi::bridge", "Downstream server returned error for textDocument/completion: {}", error);
    }
    let result = response.get_mut("result").map(serde_json::Value::take)?;
    if result.is_null() {
        return None;
    }

    // Determine format and deserialize into a unified CompletionList
    let mut list = if result.is_array() {
        // Legacy format: array of CompletionItem. Normalize to CompletionList.
        let Ok(items) = serde_json::from_value::<Vec<CompletionItem>>(result) else {
            return None;
        };
        CompletionList {
            is_incomplete: false,
            items,
        }
    } else {
        // Preferred format: CompletionList object
        let Ok(list) = serde_json::from_value::<CompletionList>(result) else {
            return None;
        };
        list
    };

    // Transform all items in the list, then optionally envelope for resolve routing
    for item in &mut list.items {
        transform_completion_item(item, offset);
        if let Some(ref ctx) = envelope_ctx {
            envelope_item_data(item, ctx);
        }
    }

    Some(list)
}

/// Transform textEdit range in a single completion item to host coordinates.
///
/// Handles both TextEdit format and InsertReplaceEdit format. Also transforms
/// additionalTextEdits if present.
pub(super) fn transform_completion_item(item: &mut CompletionItem, offset: RegionOffset) {
    // Transform text_edit if present
    if let Some(ref mut text_edit) = item.text_edit {
        match text_edit {
            tower_lsp_server::ls_types::CompletionTextEdit::Edit(edit) => {
                translate_virtual_range_to_host(&mut edit.range, offset);
            }
            tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(edit) => {
                translate_virtual_range_to_host(&mut edit.insert, offset);
                translate_virtual_range_to_host(&mut edit.replace, offset);
            }
        }
    }

    // Transform additional_text_edits if present
    if let Some(ref mut additional_edits) = item.additional_text_edits {
        for edit in additional_edits {
            translate_virtual_range_to_host(&mut edit.range, offset);
        }
    }
}

// =============================================================================
// Envelope types for completionItem/resolve routing
// =============================================================================

/// Wrapper key inside `CompletionItem.data` that identifies the origin server.
const ENVELOPE_KEY: &str = "kakehashi";

/// Envelope stored in `CompletionItem.data` for routing `completionItem/resolve`.
///
/// When Kakehashi fans out completion to multiple downstream servers, each item's
/// `data` field is wrapped in this envelope so that a later `completionItem/resolve`
/// can be routed back to the correct server.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct KakehashiEnvelope {
    /// Server name identifying which downstream produced the item.
    pub origin: String,
    /// The downstream server's original `data` value (preserved verbatim).
    pub inner: Option<Value>,
    /// Region offset snapshot for coordinate transformation of the resolved response.
    pub offset: EnvelopeOffset,
}

/// Offset snapshot stored in the envelope for coordinate back-translation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub(crate) struct EnvelopeOffset {
    pub line: u32,
    pub column: u32,
}

impl From<RegionOffset> for EnvelopeOffset {
    fn from(o: RegionOffset) -> Self {
        Self {
            line: o.line,
            column: o.column,
        }
    }
}

impl From<&EnvelopeOffset> for RegionOffset {
    fn from(o: &EnvelopeOffset) -> Self {
        Self {
            line: o.line,
            column: o.column,
        }
    }
}

/// Context needed to create envelopes during completion response processing.
pub(crate) struct EnvelopeContext<'a> {
    pub server_name: &'a str,
    pub offset: RegionOffset,
}

/// Wrap `item.data` in a Kakehashi envelope for origin tracking.
///
/// The original `data` value is moved into `inner`, and the envelope is
/// serialized as `{"kakehashi": {...}}`.
pub(crate) fn envelope_item_data(item: &mut CompletionItem, ctx: &EnvelopeContext) {
    let inner = item.data.take();
    let envelope = KakehashiEnvelope {
        origin: ctx.server_name.to_string(),
        inner,
        offset: EnvelopeOffset::from(ctx.offset),
    };
    item.data = Some(serde_json::json!({ ENVELOPE_KEY: envelope }));
}

/// Extract the envelope from a completion item's `data` without modifying the item.
///
/// Returns `None` if `data` is absent or not an envelope.
pub(crate) fn extract_envelope(item: &CompletionItem) -> Option<KakehashiEnvelope> {
    let data = item.data.as_ref()?;
    let wrapper = data.get(ENVELOPE_KEY)?;
    serde_json::from_value(wrapper.clone()).ok()
}

/// Extract the envelope and restore the original `data` value.
///
/// On success, `item.data` is set back to the downstream's original value (`inner`).
/// Returns the extracted envelope. Returns `None` if not an envelope (item unchanged).
pub(crate) fn strip_envelope(item: &mut CompletionItem) -> Option<KakehashiEnvelope> {
    let envelope = extract_envelope(item)?;
    item.data = envelope.inner.clone();
    Some(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;
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
    // Completion request tests
    // ==========================================================================

    #[test]
    fn completion_request_uses_virtual_uri() {
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_completion_request(
            &virtual_uri,
            test_position(),
            RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_uses_virtual_uri(&request, "lua");
    }

    #[test]
    fn completion_request_translates_position_to_virtual_coordinates() {
        // Host line 5, region starts at line 3 -> virtual line 2
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let request = build_completion_request(
            &virtual_uri,
            test_position(),
            RegionOffset::new(3, 0),
            test_request_id(),
        );

        assert_position_request(&request, "textDocument/completion", 2);
    }

    // ==========================================================================
    // Completion response transformation tests
    // ==========================================================================

    #[test]
    fn completion_response_transforms_textedit_ranges() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [
                    {
                        "label": "print",
                        "kind": 3,
                        "textEdit": {
                            "range": {
                                "start": { "line": 1, "character": 0 },
                                "end": { "line": 1, "character": 3 }
                            },
                            "newText": "print"
                        }
                    },
                    { "label": "pairs", "kind": 3 }
                ]
            }
        });
        let region_start_line = 3;

        let transformed = transform_completion_response_to_host(
            response,
            RegionOffset::new(region_start_line, 0),
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        assert_eq!(list.items.len(), 2);

        // First item should have transformed range
        let item = &list.items[0];
        assert_eq!(item.label, "print");
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) = item.text_edit
        {
            assert_eq!(edit.range.start.line, 4); // 1 + 3 = 4
            assert_eq!(edit.range.end.line, 4);
        } else {
            panic!("Expected TextEdit");
        }

        // Second item has no textEdit
        assert_eq!(list.items[1].label, "pairs");
        assert!(list.items[1].text_edit.is_none());
    }

    #[rstest]
    #[case::null_result(json!({"jsonrpc": "2.0", "id": 42, "result": null}))]
    #[case::without_result(json!({"jsonrpc": "2.0", "id": 42}))]
    #[case::malformed_result(json!({"jsonrpc": "2.0", "id": 42, "result": "not_a_completion_response"}))]
    #[case::error_response(json!({"jsonrpc": "2.0", "id": 42, "error": {"code": -32600, "message": "Invalid Request"}}))]
    fn completion_response_returns_none_for_invalid_response(#[case] response: serde_json::Value) {
        let transformed =
            transform_completion_response_to_host(response, RegionOffset::new(3, 0), None);
        assert!(transformed.is_none());
    }

    #[test]
    fn completion_response_handles_array_format() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "print",
                "textEdit": {
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 2 }
                    },
                    "newText": "print"
                }
            }]
        });
        let region_start_line = 5;

        let transformed = transform_completion_response_to_host(
            response,
            RegionOffset::new(region_start_line, 0),
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        // Array format is normalized to CompletionList with isIncomplete=false
        assert!(!list.is_incomplete);
        assert_eq!(list.items.len(), 1);

        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 5); // 0 + 5 = 5
            assert_eq!(edit.range.end.line, 5);
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_transforms_insert_replace_edit() {
        // InsertReplaceEdit format used by rust-analyzer, tsserver, etc.
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "println!",
                    "textEdit": {
                        "insert": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 3 }
                        },
                        "replace": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 8 }
                        },
                        "newText": "println!"
                    }
                }]
            }
        });
        let region_start_line = 10;

        let transformed = transform_completion_response_to_host(
            response,
            RegionOffset::new(region_start_line, 0),
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();

        let item = &list.items[0];
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(ref edit)) =
            item.text_edit
        {
            // Insert range transformed: line 2 + 10 = 12
            assert_eq!(edit.insert.start.line, 12);
            assert_eq!(edit.insert.end.line, 12);
            // Replace range transformed: line 2 + 10 = 12
            assert_eq!(edit.replace.start.line, 12);
            assert_eq!(edit.replace.end.line, 12);
        } else {
            panic!("Expected InsertReplaceEdit");
        }
    }

    #[test]
    fn completion_response_transforms_additional_text_edits() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "import",
                    "additionalTextEdits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "import module\n"
                    }]
                }]
            }
        });
        let region_start_line = 5;

        let transformed = transform_completion_response_to_host(
            response,
            RegionOffset::new(region_start_line, 0),
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();

        let item = &list.items[0];
        assert!(item.additional_text_edits.is_some());
        let edits = item.additional_text_edits.as_ref().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].range.start.line, 5); // 0 + 5 = 5
    }

    // ==========================================================================
    // Column offset tests for completion response transformation
    // ==========================================================================

    #[test]
    fn completion_response_applies_column_offset_on_first_virtual_line() {
        // Item on virtual line 0 → column offset added to character positions
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "func",
                    "textEdit": {
                        "range": {
                            "start": { "line": 0, "character": 2 },
                            "end": { "line": 0, "character": 5 }
                        },
                        "newText": "function"
                    }
                }]
            }
        });

        let transformed =
            transform_completion_response_to_host(response, RegionOffset::new(10, 4), None);

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 10); // 0 + 10
            assert_eq!(edit.range.start.character, 6); // 2 + 4 (first line)
            assert_eq!(edit.range.end.line, 10);
            assert_eq!(edit.range.end.character, 9); // 5 + 4 (first line)
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_ignores_column_offset_on_non_first_virtual_line() {
        // Item on virtual line 1 → only line offset, character unchanged
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "func",
                    "textEdit": {
                        "range": {
                            "start": { "line": 1, "character": 2 },
                            "end": { "line": 1, "character": 5 }
                        },
                        "newText": "function"
                    }
                }]
            }
        });

        let transformed =
            transform_completion_response_to_host(response, RegionOffset::new(10, 4), None);

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, 11); // 1 + 10
            assert_eq!(edit.range.start.character, 2); // unchanged
            assert_eq!(edit.range.end.line, 11);
            assert_eq!(edit.range.end.character, 5); // unchanged
        } else {
            panic!("Expected TextEdit");
        }
    }

    #[test]
    fn completion_response_column_offset_with_insert_replace_edit() {
        // InsertReplaceEdit on virtual line 0 → column offset on both ranges
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "println!",
                    "textEdit": {
                        "insert": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 6 }
                        },
                        "replace": {
                            "start": { "line": 0, "character": 3 },
                            "end": { "line": 0, "character": 10 }
                        },
                        "newText": "println!"
                    }
                }]
            }
        });

        let transformed =
            transform_completion_response_to_host(response, RegionOffset::new(5, 7), None);

        let list = transformed.unwrap();
        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::InsertAndReplace(ref edit)) =
            list.items[0].text_edit
        {
            // Insert range: line 0→5, char +7
            assert_eq!(edit.insert.start.line, 5);
            assert_eq!(edit.insert.start.character, 10); // 3 + 7
            assert_eq!(edit.insert.end.line, 5);
            assert_eq!(edit.insert.end.character, 13); // 6 + 7
            // Replace range: line 0→5, char +7
            assert_eq!(edit.replace.start.line, 5);
            assert_eq!(edit.replace.start.character, 10); // 3 + 7
            assert_eq!(edit.replace.end.line, 5);
            assert_eq!(edit.replace.end.character, 17); // 10 + 7
        } else {
            panic!("Expected InsertReplaceEdit");
        }
    }

    #[test]
    fn completion_response_column_offset_with_additional_text_edits_on_first_line() {
        // additionalTextEdits on virtual line 0 → column offset applied
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "isIncomplete": false,
                "items": [{
                    "label": "import",
                    "additionalTextEdits": [{
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 0 }
                        },
                        "newText": "use std::io;\n"
                    }]
                }]
            }
        });

        let transformed =
            transform_completion_response_to_host(response, RegionOffset::new(5, 3), None);

        let list = transformed.unwrap();
        let edits = list.items[0].additional_text_edits.as_ref().unwrap();
        assert_eq!(edits[0].range.start.line, 5);
        assert_eq!(edits[0].range.start.character, 3); // 0 + 3
        assert_eq!(edits[0].range.end.line, 5);
        assert_eq!(edits[0].range.end.character, 3); // 0 + 3
    }

    #[test]
    fn completion_request_applies_column_offset_on_first_line() {
        // Host position on first line of region → column subtracted in request
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 5,
            character: 14,
        };
        let request = build_completion_request(
            &virtual_uri,
            host_pos,
            RegionOffset::new(5, 4),
            test_request_id(),
        );

        assert_eq!(request["params"]["position"]["line"], 0); // 5 - 5
        assert_eq!(request["params"]["position"]["character"], 10); // 14 - 4
    }

    #[test]
    fn completion_request_ignores_column_offset_on_non_first_line() {
        // Host position on non-first line → column NOT subtracted
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 7,
            character: 14,
        };
        let request = build_completion_request(
            &virtual_uri,
            host_pos,
            RegionOffset::new(5, 4),
            test_request_id(),
        );

        assert_eq!(request["params"]["position"]["line"], 2); // 7 - 5
        assert_eq!(request["params"]["position"]["character"], 14); // unchanged
    }

    #[test]
    fn completion_range_transformation_saturates_on_overflow() {
        // Test defensive arithmetic: saturating_add prevents panic on overflow
        let response = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": [{
                "label": "test",
                "textEdit": {
                    "range": {
                        "start": { "line": 4294967295u32, "character": 0 },  // u32::MAX
                        "end": { "line": 4294967295u32, "character": 5 }
                    },
                    "newText": "test"
                }
            }]
        });
        let region_start_line = 10;

        let transformed = transform_completion_response_to_host(
            response,
            RegionOffset::new(region_start_line, 0),
            None,
        );

        assert!(transformed.is_some());
        let list = transformed.unwrap();
        // Array format is normalized to CompletionList with isIncomplete=false
        assert!(!list.is_incomplete);

        if let Some(tower_lsp_server::ls_types::CompletionTextEdit::Edit(ref edit)) =
            list.items[0].text_edit
        {
            assert_eq!(edit.range.start.line, u32::MAX);
            assert_eq!(edit.range.end.line, u32::MAX);
        } else {
            panic!("Expected TextEdit");
        }
    }

    // ==========================================================================
    // Envelope round-trip tests
    // ==========================================================================

    fn test_envelope_ctx() -> EnvelopeContext<'static> {
        EnvelopeContext {
            server_name: "lua-ls",
            offset: RegionOffset::new(3, 4),
        }
    }

    #[test]
    fn envelope_round_trip_with_data() {
        let mut item = CompletionItem {
            label: "print".to_string(),
            data: Some(json!({"resolve_id": 42})),
            ..Default::default()
        };
        let ctx = test_envelope_ctx();
        envelope_item_data(&mut item, &ctx);

        // data should now be wrapped
        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert_eq!(envelope.origin, "lua-ls");
        assert_eq!(envelope.inner, Some(json!({"resolve_id": 42})));
        assert_eq!(envelope.offset, EnvelopeOffset { line: 3, column: 4 });

        // strip restores original data
        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.origin, "lua-ls");
        assert_eq!(item.data, Some(json!({"resolve_id": 42})));
    }

    #[test]
    fn envelope_round_trip_with_none_data() {
        let mut item = CompletionItem {
            label: "pairs".to_string(),
            data: None,
            ..Default::default()
        };
        let ctx = test_envelope_ctx();
        envelope_item_data(&mut item, &ctx);

        let envelope = extract_envelope(&item).expect("should extract envelope");
        assert_eq!(envelope.inner, None);

        let stripped = strip_envelope(&mut item).expect("should strip");
        assert_eq!(stripped.inner, None);
        assert_eq!(item.data, None);
    }

    #[test]
    fn extract_envelope_returns_none_for_non_envelope_data() {
        let item = CompletionItem {
            label: "test".to_string(),
            data: Some(json!({"some_other_key": "value"})),
            ..Default::default()
        };
        assert!(extract_envelope(&item).is_none());
    }

    #[test]
    fn extract_envelope_returns_none_for_no_data() {
        let item = CompletionItem {
            label: "test".to_string(),
            data: None,
            ..Default::default()
        };
        assert!(extract_envelope(&item).is_none());
    }

    #[test]
    fn strip_envelope_leaves_non_envelope_data_unchanged() {
        let original_data = json!({"custom": true});
        let mut item = CompletionItem {
            label: "test".to_string(),
            data: Some(original_data.clone()),
            ..Default::default()
        };
        assert!(strip_envelope(&mut item).is_none());
        assert_eq!(item.data, Some(original_data));
    }
}
