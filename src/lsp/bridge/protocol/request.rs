//! Request builders for LSP bridge communication.
//!
//! This module provides functions to build JSON-RPC requests for downstream
//! language servers with proper coordinate translation from host to virtual
//! document coordinates.

use tower_lsp_server::ls_types::{
    DidOpenTextDocumentParams, NumberOrString, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri,
};

use super::jsonrpc::{JsonRpcNotification, JsonRpcRequest};
use super::request_id::RequestId;
use super::translation::{RegionOffset, translate_host_position_to_virtual};
use super::virtual_uri::VirtualDocumentUri;

/// A position-based request's params plus an optional client-provided
/// `workDoneToken` (ls-bridge-client-progress). `TextDocumentPositionParams` has
/// no progress field, so we flatten it and add the token alongside; when the
/// token is `None` this serializes byte-identically to the bare position params.
#[derive(Debug, serde::Serialize)]
pub(crate) struct PositionRequestParams {
    #[serde(flatten)]
    position: TextDocumentPositionParams,
    #[serde(rename = "workDoneToken", skip_serializing_if = "Option::is_none")]
    work_done_token: Option<NumberOrString>,
}

/// Build `TextDocumentPositionParams` with host-to-virtual coordinate translation.
///
/// Shared helper for all position-translating request builders (hover, completion,
/// definition, references, rename, …). Uses `saturating_sub` for line translation
/// to prevent a panic on underflow.
pub(crate) fn build_text_document_position_params(
    virtual_uri: &VirtualDocumentUri,
    host_position: Position,
    offset: &RegionOffset,
) -> TextDocumentPositionParams {
    let mut virtual_position = host_position;
    translate_host_position_to_virtual(&mut virtual_position, offset);

    TextDocumentPositionParams::new(
        TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
        virtual_position,
    )
}

/// Build a position-based JSON-RPC request (`hover`, `completion`, `definition`, …)
/// for a downstream server: translate `host_position` via `offset` and wrap it
/// for `method`.
///
/// Line translation uses `saturating_sub` so an in-flight request whose region
/// got invalidated by a concurrent edit clamps to line 0 — wrong result, not
/// a panic.
pub(crate) fn build_position_based_request(
    virtual_uri: &VirtualDocumentUri,
    host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
    method: &'static str,
) -> JsonRpcRequest<TextDocumentPositionParams> {
    let params = build_text_document_position_params(virtual_uri, host_position, offset);
    JsonRpcRequest::new(request_id.as_i64(), method, params)
}

/// Like [`build_position_based_request`], but carries the editor's
/// `workDoneToken` so the downstream reports `$/progress` against it
/// (ls-bridge-client-progress). `client_progress_token = None` is equivalent to
/// [`build_position_based_request`] (the token field is omitted).
pub(crate) fn build_position_based_request_with_progress(
    virtual_uri: &VirtualDocumentUri,
    host_position: Position,
    offset: &RegionOffset,
    request_id: RequestId,
    method: &'static str,
    client_progress_token: Option<NumberOrString>,
) -> JsonRpcRequest<PositionRequestParams> {
    let position = build_text_document_position_params(virtual_uri, host_position, offset);
    JsonRpcRequest::new(
        request_id.as_i64(),
        method,
        PositionRequestParams {
            position,
            work_done_token: client_progress_token,
        },
    )
}

/// Build a whole-document JSON-RPC request (documentLink, documentSymbol,
/// documentColor, …) that operates on an entire document without a position.
pub(crate) fn build_whole_document_request(
    virtual_uri: &VirtualDocumentUri,
    request_id: RequestId,
    method: &'static str,
) -> JsonRpcRequest<DocumentParams> {
    let params = DocumentParams {
        text_document: TextDocumentIdentifier {
            uri: virtual_uri_to_lsp_uri(virtual_uri),
        },
    };

    JsonRpcRequest::new(request_id.as_i64(), method, params)
}

/// Build a JSON-RPC didOpen notification carrying a virtual document's initial
/// content to a downstream language server.
pub(crate) fn build_didopen_notification(
    virtual_uri: &VirtualDocumentUri,
    content: &str,
) -> JsonRpcNotification<DidOpenTextDocumentParams> {
    let params = DidOpenTextDocumentParams {
        text_document: TextDocumentItem::new(
            virtual_uri_to_lsp_uri(virtual_uri),
            virtual_uri.language().to_string(),
            1,
            content.to_string(),
        ),
    };

    JsonRpcNotification::new("textDocument/didOpen", params)
}

/// Params for requests that only need a text document identifier.
///
/// Used by document-wide requests (documentLink, documentSymbol, documentColor, etc.)
/// where the LSP spec defines different params types per method but the bridge
/// only sends the `textDocument` field.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DocumentParams {
    text_document: TextDocumentIdentifier,
}

/// Convert a `VirtualDocumentUri` to a `ls_types::Uri`.
///
/// Delegates to [`VirtualDocumentUri::to_lsp_uri()`] which handles parse
/// failures gracefully (logs + falls back to host URI).
pub(crate) fn virtual_uri_to_lsp_uri(virtual_uri: &VirtualDocumentUri) -> Uri {
    virtual_uri.to_lsp_uri()
}

#[cfg(test)]
mod tests {
    use super::super::super::text_document::test_helpers::test_host_uri;
    use super::*;
    use tower_lsp_server::ls_types::Position;

    #[test]
    fn position_request_first_line_applies_column_offset() {
        // Host position on first line of region → column offset subtracted
        // Host line 5, char 10; region starts at line 5, col 4
        // → virtual line 0, virtual char 6 (10 - 4)
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 5,
            character: 10,
        };
        let request = build_position_based_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            "textDocument/completion",
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 0);
        assert_eq!(json["params"]["position"]["character"], 6); // 10 - 4
    }

    #[test]
    fn position_request_non_first_line_ignores_column_offset() {
        // Host position on non-first line of region → column offset NOT applied
        // Host line 7, char 10; region starts at line 5, col 4
        // → virtual line 2, virtual char 10 (unchanged)
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 7,
            character: 10,
        };
        let request = build_position_based_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            "textDocument/completion",
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 2); // 7 - 5
        assert_eq!(json["params"]["position"]["character"], 10); // unchanged
    }

    #[test]
    fn position_request_column_offset_saturates_on_underflow() {
        // Host character < region_start_column → saturating_sub gives 0
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 5,
            character: 2,
        };
        let request = build_position_based_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 10),
            RequestId::new(1),
            "textDocument/completion",
        );

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["params"]["position"]["line"], 0);
        assert_eq!(json["params"]["position"]["character"], 0); // saturated
    }

    #[test]
    fn progress_request_includes_work_done_token_only_when_present() {
        use tower_lsp_server::ls_types::NumberOrString;
        let virtual_uri = VirtualDocumentUri::new(&test_host_uri(), "lua", "region-0");
        let host_pos = Position {
            line: 5,
            character: 4,
        };

        // With a token: `workDoneToken` is present alongside the (flattened)
        // position params.
        let with = build_position_based_request_with_progress(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            "textDocument/definition",
            Some(NumberOrString::String("wd-1".to_string())),
        );
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!(json["params"]["workDoneToken"], "wd-1");
        assert!(
            json["params"]["textDocument"]["uri"].is_string(),
            "position params still present"
        );
        assert!(json["params"]["position"]["line"].is_number());

        // Without a token: the field is omitted entirely, leaving the params
        // byte-identical to the bare position request (non-regression).
        let without = build_position_based_request_with_progress(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            "textDocument/definition",
            None,
        );
        let bare = build_position_based_request(
            &virtual_uri,
            host_pos,
            &RegionOffset::new(5, 4),
            RequestId::new(1),
            "textDocument/definition",
        );
        assert!(
            serde_json::to_value(&without).unwrap()["params"]
                .get("workDoneToken")
                .is_none(),
            "workDoneToken omitted when None"
        );
        assert_eq!(
            serde_json::to_value(&without).unwrap(),
            serde_json::to_value(&bare).unwrap(),
            "None serializes identically to the bare position request"
        );
    }
}
