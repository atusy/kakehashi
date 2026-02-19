//! E2E tests: requests outside injection regions return null.
//!
//! When the cursor position (or range) falls outside any embedded code block,
//! kakehashi short-circuits the request and returns `null` without forwarding
//! to the downstream language server. Each `#[case]` tests a different LSP
//! method with its specific parameter shape.
//!
//! Run with: `cargo test --test e2e_lsp_outside_injection --features e2e`

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use rstest::rstest;
use serde_json::json;

#[rstest]
#[case::declaration("textDocument/declaration", |uri: &str| json!({
    "textDocument": { "uri": uri },
    "position": { "line": 2, "character": 5 }
}))]
#[case::document_highlight("textDocument/documentHighlight", |uri: &str| json!({
    "textDocument": { "uri": uri },
    "position": { "line": 2, "character": 5 }
}))]
#[case::implementation("textDocument/implementation", |uri: &str| json!({
    "textDocument": { "uri": uri },
    "position": { "line": 2, "character": 5 }
}))]
#[case::inlay_hint("textDocument/inlayHint", |uri: &str| json!({
    "textDocument": { "uri": uri },
    "range": {
        "start": { "line": 2, "character": 0 },
        "end": { "line": 3, "character": 0 }
    }
}))]
#[case::references("textDocument/references", |uri: &str| json!({
    "textDocument": { "uri": uri },
    "position": { "line": 2, "character": 5 },
    "context": { "includeDeclaration": true }
}))]
#[case::rename("textDocument/rename", |uri: &str| json!({
    "textDocument": { "uri": uri },
    "position": { "line": 2, "character": 5 },
    "newName": "newName"
}))]
fn e2e_outside_injection_returns_null(
    #[case] method: &str,
    #[case] build_params: fn(&str) -> serde_json::Value,
) {
    let mut client = create_lua_configured_client();

    let markdown_content = r#"# Test Document

Some text before the code block.

```lua
local x = 42
print(x)
```

More text after.
"#;

    let markdown_uri = "file:///test_outside_injection.md";

    client.send_notification(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": markdown_uri,
                "languageId": "markdown",
                "version": 1,
                "text": markdown_content
            }
        }),
    );

    let response = client.send_request(method, build_params(markdown_uri));

    assert!(
        response.get("error").is_none(),
        "{method} should not return error: {:?}",
        response.get("error")
    );

    let result = response.get("result");
    assert!(
        result.is_some() && result.unwrap().is_null(),
        "{method} outside injection region should return null, got: {:?}",
        result
    );

    shutdown_client(&mut client);
}
