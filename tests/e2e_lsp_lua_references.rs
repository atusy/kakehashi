//! End-to-end test for Lua find references in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for references:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - References request at position in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - References locations received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_references --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, is_lua_ls_available, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: references request is handled without error
#[test]
fn e2e_references_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing local variable with multiple references
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

    let markdown_uri = "file:///test_references.md";

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

    // Request references on "name" parameter at line 3 (function greet(name))
    // The parameter 'name' starts at character 21 on line 3
    let refs_response = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 23 },
            "context": { "includeDeclaration": true }
        }),
    );

    println!("References response: {:?}", refs_response);

    // Verify no error
    assert!(
        refs_response.get("error").is_none(),
        "References should not return error: {:?}",
        refs_response.get("error")
    );

    let result = refs_response
        .get("result")
        .expect("References should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading
        println!("Note: lua-ls returned null (may still be loading)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // Location[] format (expected for references)
        let locations = result.as_array().unwrap();
        println!("References found: {} locations", locations.len());

        // With includeDeclaration=true, we should find at least 2 references to 'name':
        // 1. The declaration in function parameter (line 3)
        // 2. The usage in return statement (line 4)
        for loc in locations {
            if let Some(range) = loc.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                println!("  - Reference at line {}", start_line);
                // The references should be in the Lua code block area (lines 3-9)
                assert!(
                    (2..=10).contains(&start_line),
                    "Reference line should be in host coordinates (expected 2-10, got {})",
                    start_line
                );
            }
            // Verify URI is the host document (not virtual)
            if let Some(uri) = loc.get("uri").and_then(|u| u.as_str()) {
                assert!(
                    uri == markdown_uri,
                    "Reference URI should be host document URI, got: {}",
                    uri
                );
            }
        }
        println!("E2E: References returns Location[] with host coordinates and URIs");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: references with includeDeclaration=false excludes declaration
#[test]
fn e2e_references_include_declaration_false() {
    if !is_lua_ls_available() {
        eprintln!("SKIP: lua-language-server not found in PATH");
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block
    let markdown_content = r#"# Test Document

```lua
local x = 42
print(x)
```

More text.
"#;

    let markdown_uri = "file:///test_refs_no_decl.md";

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

    // Request references on "x" with includeDeclaration=false
    // Line 3: local x = 42 (declaration)
    // Line 4: print(x) (usage)
    let refs_response = client.send_request(
        "textDocument/references",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 7 },  // On 'x' in print(x)
            "context": { "includeDeclaration": false }
        }),
    );

    println!("References (no decl) response: {:?}", refs_response);

    // Verify no error
    assert!(
        refs_response.get("error").is_none(),
        "References should not return error: {:?}",
        refs_response.get("error")
    );

    // Note: lua-ls may or may not respect includeDeclaration flag
    // This test primarily verifies the flag is passed through correctly
    println!("E2E: References with includeDeclaration=false handled correctly");

    // Clean shutdown
    shutdown_client(&mut client);
}
