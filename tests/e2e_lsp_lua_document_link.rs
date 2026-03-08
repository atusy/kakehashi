//! End-to-end test for Lua document link in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for document link:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Document link request sent
//! - kakehashi detects injection, spawns lua-ls, and transforms coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_document_link --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//! **Note**: lua-ls may not support documentLink (returns method not found), so
//! this test mainly verifies the kakehashi infrastructure is wired correctly.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{create_lua_configured_client, shutdown_client};
use serde_json::json;

/// E2E test: document link request is handled without error
#[test]
fn e2e_document_link_request_handled() {
    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing require statement
    let markdown_content = r#"# Test Document

```lua
local json = require("cjson")
local data = json.decode('{"key": "value"}')
print(data.key)
```

More text.
"#;

    let markdown_uri = "file:///test_document_link.md";

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
    std::thread::sleep(std::time::Duration::from_millis(1500));

    // Request document link for the file
    let link_response = client.send_request(
        "textDocument/documentLink",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!("Document link response: {:?}", link_response);

    // The request should complete without crashing
    // lua-ls may return:
    // - null (no links found)
    // - [] (empty array)
    // - array of DocumentLink objects
    // - error (method not supported by lua-ls)

    // All of these are valid - the important thing is kakehashi handled the request
    assert!(
        link_response.get("id").is_some(),
        "Response should have id field"
    );

    // Check that we didn't get an internal error from kakehashi itself
    if let Some(error) = link_response.get("error") {
        // Method not found (-32601) from downstream is acceptable
        // Internal errors from kakehashi would be different codes
        let code = error.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        if code != -32601 {
            // -32601 is "method not found" which is OK (lua-ls doesn't support documentLink)
            panic!("Unexpected error: {:?}", error);
        }
        println!("E2E: lua-ls returned method not found (expected - documentLink not supported)");
    } else {
        // Got a result (null or array) - either is fine
        let result = link_response.get("result");
        if let Some(r) = result {
            if r.is_array() {
                let links = r.as_array().unwrap();
                println!("E2E: Got {} document links", links.len());

                // If we got links, verify they have been transformed to host coordinates
                for link in links {
                    if let Some(range) = link.get("range") {
                        let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                        // Links should be in host coordinates (inside the code block, line 3+)
                        assert!(
                            start_line >= 2,
                            "Link line should be in host coordinates (expected >= 2, got {})",
                            start_line
                        );
                    }
                }
            } else if r.is_null() {
                println!("E2E: Got null result (no links found)");
            }
        }
    }

    println!("E2E: Document link request completed successfully");

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: document link for markdown file without code blocks returns null
#[test]
fn e2e_document_link_no_injections_returns_null() {
    let mut client = create_lua_configured_client();

    // Open markdown document WITHOUT code blocks
    let markdown_content = r#"# Test Document

Just some plain text without any code blocks.

More text here.
"#;

    let markdown_uri = "file:///test_no_injections.md";

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

    // Request document link
    let link_response = client.send_request(
        "textDocument/documentLink",
        json!({
            "textDocument": { "uri": markdown_uri }
        }),
    );

    println!(
        "Document link (no injections) response: {:?}",
        link_response
    );

    // Should return null since there are no injection regions
    assert!(
        link_response.get("error").is_none(),
        "Should not return error for markdown without injections"
    );

    let result = link_response.get("result");
    assert!(
        result.is_some() && result.unwrap().is_null(),
        "Document link for markdown without injections should return null"
    );

    println!("E2E: Document link correctly returns null for markdown without code blocks");

    // Clean shutdown
    shutdown_client(&mut client);
}
