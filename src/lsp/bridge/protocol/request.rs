//! Request builders for LSP bridge communication.
//!
//! This module provides functions to build JSON-RPC requests for downstream
//! language servers with proper coordinate translation from host to virtual
//! document coordinates.

use tower_lsp_server::ls_types::{
    DidOpenTextDocumentParams, Position, TextDocumentIdentifier, TextDocumentItem,
    TextDocumentPositionParams, Uri,
};

use super::jsonrpc::{JsonRpcNotification, JsonRpcRequest};
use super::request_id::RequestId;
use super::translation::{RegionOffset, translate_host_position_to_virtual};
use super::virtual_uri::VirtualDocumentUri;

/// Build `TextDocumentPositionParams` with host-to-virtual coordinate translation.
///
/// This is the shared helper for all request builders that need position
/// translation (hover, completion, definition, references, rename, etc.).
///
/// # Defensive Arithmetic
///
/// Uses `saturating_sub` for line translation to prevent panic on underflow.
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

/// Build a position-based JSON-RPC request for a downstream language server.
///
/// This is the core helper for building LSP requests that operate on a position
/// (hover, completion, definition, etc.). It handles:
/// - Translating host position to virtual coordinates
/// - Building the JSON-RPC request structure
///
/// # Arguments
/// * `virtual_uri` - The pre-built virtual document URI
/// * `host_position` - The position in the host document
/// * `offset` - The region offset for coordinate translation
/// * `request_id` - The JSON-RPC request ID
/// * `method` - The LSP method name (e.g., "textDocument/hover")
///
/// # Defensive Arithmetic
///
/// Uses `saturating_sub` for line translation to prevent panic on underflow.
/// This can occur during race conditions when document edits invalidate region
/// data while an LSP request is in flight. In such cases, the request will use
/// line 0, which may produce incorrect results but won't crash the server.
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

/// Build a whole-document JSON-RPC request for a downstream language server.
///
/// This is the core helper for building LSP requests that operate on an entire
/// document without position (documentLink, documentSymbol, documentColor, etc.).
/// It handles:
/// - Building the JSON-RPC request structure with just textDocument
///
/// # Arguments
/// * `virtual_uri` - The pre-built virtual document URI
/// * `request_id` - The JSON-RPC request ID
/// * `method` - The LSP method name (e.g., "textDocument/documentLink")
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

/// Build a JSON-RPC didOpen notification for a downstream language server.
///
/// Sends the initial document content to the downstream language server when
/// a virtual document is first opened.
///
/// # Arguments
/// * `virtual_uri` - The virtual document URI
/// * `content` - The initial content of the virtual document
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
pub(crate) struct DocumentParams {
    #[serde(rename = "textDocument")]
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
    // ==========================================================================
    // Test helpers
    // ==========================================================================

    /// Assert that a request uses a virtual URI with the expected extension.
    fn assert_uses_virtual_uri(request: &impl serde::Serialize, extension: &str) {
        let request = serde_json::to_value(request).unwrap();
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

    #[test]
    fn assert_uses_virtual_uri_handles_fragments() {
        // URIs with fragments (e.g., vscode-notebook-cell://) preserve the fragment
        // The helper should correctly detect the extension before the fragment
        let request = serde_json::json!({
            "params": {
                "textDocument": {
                    "uri": "vscode-notebook-cell://authority/path/kakehashi-virtual-uri-REGION.py#cell-id"
                }
            }
        });

        // This should pass - the extension is .py even though URI ends with #cell-id
        assert_uses_virtual_uri(&request, "py");
    }

    // ==========================================================================
    // Column offset tests for build_position_based_request
    // ==========================================================================

    use super::*;
    use tower_lsp_server::ls_types::Position;

    fn test_host_uri() -> tower_lsp_server::ls_types::Uri {
        let url = url::Url::parse("file:///project/doc.md").unwrap();
        crate::lsp::lsp_impl::url_to_uri(&url).expect("test URL should convert to URI")
    }

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
}
