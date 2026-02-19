//! End-to-end test for document color in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for document color:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Document color request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_document_color --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//!
//! **Note**: lua-language-server typically doesn't return color information for Lua code,
//! so this test verifies the bridge infrastructure works correctly by accepting empty results.

#![cfg(all(feature = "e2e", feature = "experimental"))]

mod helpers;

use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;

/// E2E test: documentColor request is handled without error
#[test]
fn e2e_document_color_request_handled() {
    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block
    // Note: lua-language-server typically doesn't provide colors for Lua code,
    // but CSS in Lua string literals might trigger color detection in some configurations
    let markdown_content = r##"# Test Document

```lua
local colors = {
    red = "#ff0000",
    green = "#00ff00",
    blue = "#0000ff"
}

print(colors.red)
```

More text.
"##;

    let markdown_uri = "file:///test_document_color.md";

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

    // Give lua-ls time to process
    std::thread::sleep(std::time::Duration::from_millis(2000));

    // Request document color for the file
    let color_response = client.send_request(
        "textDocument/documentColor",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!("Document color response: {:?}", color_response);

    // The request should complete without crashing
    assert!(
        color_response.get("id").is_some(),
        "Response should have id field"
    );

    // Check for errors
    if let Some(error) = color_response.get("error") {
        panic!("Document color request returned error: {:?}", error);
    }

    // Parse the result - can be null or an array of ColorInformation
    let result = color_response.get("result");
    if let Some(r) = result {
        if r.is_array() {
            let colors = r.as_array().unwrap();
            println!("E2E: Got {} color(s)", colors.len());

            // If colors are returned, verify they have valid structure
            for color in colors {
                // ColorInformation has range and color fields
                assert!(
                    color.get("range").is_some(),
                    "Color should have range field"
                );
                assert!(
                    color.get("color").is_some(),
                    "Color should have color field"
                );

                // If we got colors, verify range is in host coordinates
                if let Some(range) = color.get("range") {
                    let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                    // Code block starts at line 2 (0-indexed), content starts at line 3
                    assert!(
                        start_line >= 3,
                        "Color range line should be in host coordinates (expected >= 3, got {})",
                        start_line
                    );
                }
            }
        } else if r.is_null() {
            // lua-ls doesn't provide colors for Lua code - this is expected
            println!("E2E: Got null result (lua-ls doesn't provide colors for Lua)");
        }
    }

    println!("E2E: Document color request completed successfully");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: documentColor for markdown without code blocks returns empty or null
#[test]
fn e2e_document_color_no_injections_returns_empty() {
    let mut client = create_lua_configured_client();

    // Open markdown document WITHOUT code blocks
    let markdown_content = r#"# Test Document

Just some plain text without any code blocks.

More text here.
"#;

    let markdown_uri = "file:///test_no_injections_color.md";

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

    // Request document color
    let color_response = client.send_request(
        "textDocument/documentColor",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!(
        "Document color (no injections) response: {:?}",
        color_response
    );

    // Should not return an error for markdown without injections
    assert!(
        color_response.get("error").is_none(),
        "Should not return error for markdown without injections"
    );

    // Result should be null or empty array - both are valid LSP responses
    let result = color_response.get("result");
    assert!(result.is_some(), "Should have result field");
    let r = result.unwrap();
    let is_empty_or_null = r.is_null() || (r.is_array() && r.as_array().unwrap().is_empty());
    assert!(
        is_empty_or_null,
        "Document color for markdown without injections should return null or empty array, got: {:?}",
        r
    );

    println!("E2E: Document color correctly returns empty for markdown without code blocks");

    // Clean shutdown
    shutdown_client(&mut client);
}
