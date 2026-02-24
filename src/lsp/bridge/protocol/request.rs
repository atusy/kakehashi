//! Request builders for LSP bridge communication.
//!
//! This module provides functions to build JSON-RPC requests for downstream
//! language servers with proper coordinate translation from host to virtual
//! document coordinates.

use super::request_id::RequestId;
use super::translation::translate_host_position_to_virtual;
use super::virtual_uri::VirtualDocumentUri;

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
/// * `region_start_line` - The starting line of the injection region in the host document
/// * `region_start_column` - The starting column of the injection region on its first host line
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
    host_position: tower_lsp_server::ls_types::Position,
    region_start_line: u32,
    region_start_column: u32,
    request_id: RequestId,
    method: &str,
) -> serde_json::Value {
    // Translate position from host to virtual coordinates
    let mut virtual_position = host_position;
    translate_host_position_to_virtual(&mut virtual_position, region_start_line, region_start_column);

    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": method,
        "params": {
            "textDocument": {
                "uri": virtual_uri.to_uri_string()
            },
            "position": {
                "line": virtual_position.line,
                "character": virtual_position.character
            }
        }
    })
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
    method: &str,
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": request_id.as_i64(),
        "method": method,
        "params": {
            "textDocument": {
                "uri": virtual_uri.to_uri_string()
            }
        }
    })
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
) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": virtual_uri.to_uri_string(),
                "languageId": virtual_uri.language(),
                "version": 1,
                "text": content
            }
        }
    })
}

#[cfg(test)]
mod tests {
    // ==========================================================================
    // Test helpers
    // ==========================================================================

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
}
