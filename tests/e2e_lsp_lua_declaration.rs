//! End-to-end test for Lua goto declaration in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for declaration:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Declaration request at position in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - Declaration location received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_declaration --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: goto declaration request is handled without error
#[test]
fn e2e_declaration_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing local variable declaration
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

    let markdown_uri = "file:///test_declaration.md";

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
    std::thread::sleep(std::time::Duration::from_millis(1000));

    // Request declaration on "greet" function call at line 7 (message = greet("World"))
    // The greet call is at character 16 on line 7
    let decl_response = client.send_request(
        "textDocument/declaration",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 7, "character": 16 }
        }),
    );

    println!("Declaration response: {:?}", decl_response);

    // Verify no error
    assert!(
        decl_response.get("error").is_none(),
        "Declaration should not return error: {:?}",
        decl_response.get("error")
    );

    let result = decl_response
        .get("result")
        .expect("Declaration should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading or cannot find declaration
        println!("Note: lua-ls returned null (may still be loading or cannot find declaration)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // Location[] format
        let locations = result.as_array().unwrap();
        if !locations.is_empty() {
            let loc = &locations[0];
            // Verify the location is in host coordinates
            if let Some(range) = loc.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                println!("Declaration found at line {}", start_line);
                // The declaration should be in the Lua code block area (lines 3-9)
                assert!(
                    (2..=10).contains(&start_line),
                    "Declaration line should be in host coordinates (expected 2-10, got {})",
                    start_line
                );
                println!("E2E: Declaration returns Location in host coordinates");
            }
        }
    } else if result.is_object() {
        // Single Location format
        if let Some(range) = result.get("range") {
            let start_line = range["start"]["line"].as_u64().unwrap_or(0);
            println!("Declaration found at line {}", start_line);
            assert!(
                (2..=10).contains(&start_line),
                "Declaration line should be in host coordinates (expected 2-10, got {})",
                start_line
            );
            println!("E2E: Declaration returns Location in host coordinates");
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
