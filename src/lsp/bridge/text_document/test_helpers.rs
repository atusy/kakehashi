//! Shared test utilities for text_document module tests.
//!
//! Provides common helpers used across text_document submodule tests.
//! Import from submodule tests via `use super::super::test_helpers::*;`

use tower_lsp_server::ls_types::Position;

use super::super::protocol::RequestId;

/// Standard test request ID used across most tests.
pub(super) fn test_request_id() -> RequestId {
    RequestId::new(42)
}

/// Standard test host URI used across most tests.
pub(in crate::lsp::bridge) fn test_host_uri() -> tower_lsp_server::ls_types::Uri {
    let url = url::Url::parse("file:///project/doc.md").unwrap();
    crate::lsp::lsp_impl::url_to_uri(&url).expect("test URL should convert to URI")
}

/// Standard test position (line 5, character 10).
pub(super) fn test_position() -> Position {
    Position {
        line: 5,
        character: 10,
    }
}

/// Assert that a request uses a virtual URI with the expected extension.
pub(in crate::lsp::bridge) fn assert_uses_virtual_uri(
    request: &impl serde::Serialize,
    extension: &str,
) {
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

/// Assert that a position-based request has correct structure and translated coordinates.
pub(super) fn assert_position_request(
    request: &impl serde::Serialize,
    expected_method: &str,
    expected_virtual_line: u64,
) {
    let request = serde_json::to_value(request).unwrap();
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
