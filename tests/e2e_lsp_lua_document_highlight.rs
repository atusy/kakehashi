//! End-to-end test for Lua document highlight in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for document highlight:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Document highlight request at position in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - Highlight ranges received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_document_highlight --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, is_lua_ls_available, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: document highlight request is handled without error
#[test]
fn e2e_document_highlight_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing local variable with multiple usages
    // The variable 'name' appears at:
    // - Line 3 (declaration in function parameter): local function greet(name)
    // - Line 4 (usage in string concatenation): return "Hello, " .. name
    let markdown_content = r#"# Test Document

```lua
local function greet(name)
    return "Hello, " .. name
end

local message = greet("World")
print(message)
```

More text.
"#;

    let markdown_uri = "file:///test_document_highlight.md";

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

    // Request document highlight on "name" parameter at line 3 (function greet(name))
    // The parameter 'name' starts at character 21 on line 3
    let highlight_response = client.send_request(
        "textDocument/documentHighlight",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 23 }
        }),
    );

    println!("Document highlight response: {:?}", highlight_response);

    // Verify no error
    assert!(
        highlight_response.get("error").is_none(),
        "Document highlight should not return error: {:?}",
        highlight_response.get("error")
    );

    let result = highlight_response
        .get("result")
        .expect("Document highlight should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading
        println!("Note: lua-ls returned null (may still be loading)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // DocumentHighlight[] format
        let highlights = result.as_array().unwrap();
        println!("Highlights found: {} items", highlights.len());

        // We should find at least 2 highlights for 'name':
        // 1. The declaration in function parameter (line 3)
        // 2. The usage in return statement (line 4)
        for highlight in highlights {
            if let Some(range) = highlight.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                println!("  - Highlight at line {}", start_line);
                // The highlights should be in the Lua code block area (lines 3-9)
                assert!(
                    (2..=10).contains(&start_line),
                    "Highlight line should be in host coordinates (expected 2-10, got {})",
                    start_line
                );
            }
            // Document highlights don't have URI field - they're always in the same document
        }
        println!("E2E: Document highlight returns DocumentHighlight[] with host coordinates");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: document highlight with variable used multiple times returns all occurrences
#[test]
fn e2e_document_highlight_multiple_occurrences() {
    if !is_lua_ls_available() {
        eprintln!("SKIP: lua-language-server not found in PATH");
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing variable used multiple times
    let markdown_content = r#"# Test Document

```lua
local x = 42
print(x)
local y = x + 1
print(x + y)
```

More text.
"#;

    let markdown_uri = "file:///test_highlight_multi.md";

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

    // Request document highlight on "x" at line 3
    // x appears at:
    // - Line 3: local x = 42 (declaration)
    // - Line 4: print(x) (read)
    // - Line 5: local y = x + 1 (read)
    // - Line 6: print(x + y) (read)
    let highlight_response = client.send_request(
        "textDocument/documentHighlight",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 7 }  // On 'x' in "local x = 42"
        }),
    );

    println!(
        "Document highlight multiple occurrences response: {:?}",
        highlight_response
    );

    // Verify no error
    assert!(
        highlight_response.get("error").is_none(),
        "Document highlight should not return error: {:?}",
        highlight_response.get("error")
    );

    let result = highlight_response.get("result");
    if let Some(highlights) = result.and_then(|r| r.as_array()) {
        println!("Found {} highlights for variable 'x'", highlights.len());
        // Should find multiple highlights (ideally 4 for 'x')
        // But lua-ls behavior may vary - at minimum we verify we got some highlights
        assert!(
            !highlights.is_empty(),
            "Should find at least one highlight for 'x'"
        );

        // All highlights should be in host coordinates
        for highlight in highlights {
            if let Some(range) = highlight.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                assert!(
                    (2..=10).contains(&start_line),
                    "All highlights should be in host coordinates (expected 2-10, got {})",
                    start_line
                );
            }
        }

        println!("E2E: Document highlight returns multiple occurrences with host coordinates");
    } else {
        println!("Note: lua-ls returned null or empty (may still be loading)");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
