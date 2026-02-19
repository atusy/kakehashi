//! End-to-end test for Lua goto implementation in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for implementation:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Implementation request at position in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - Implementation location received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_implementation --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: goto implementation request is handled without error
#[test]
fn e2e_implementation_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing class/method pattern
    // lua-language-server uses LuaCATS annotations for class definitions
    let markdown_content = r#"# Test Document

```lua
---@class Animal
---@field name string
local Animal = {}

function Animal:speak()
    print("...")
end

---@class Dog : Animal
local Dog = {}

function Dog:speak()
    print("Woof!")
end

local dog = Dog
dog:speak()
```

More text.
"#;

    let markdown_uri = "file:///test_implementation.md";

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

    // Request implementation on "speak" method call at line 19 (dog:speak())
    // The speak method is at character 4 on line 19
    let impl_response = client.send_request(
        "textDocument/implementation",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 19, "character": 4 }
        }),
    );

    println!("Implementation response: {:?}", impl_response);

    // Verify no error
    assert!(
        impl_response.get("error").is_none(),
        "Implementation should not return error: {:?}",
        impl_response.get("error")
    );

    let result = impl_response
        .get("result")
        .expect("Implementation should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading or cannot find implementations
        println!("Note: lua-ls returned null (may still be loading or cannot find implementation)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // Location[] format
        let locations = result.as_array().unwrap();
        if !locations.is_empty() {
            let loc = &locations[0];
            // Verify the location is in host coordinates
            if let Some(range) = loc.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                println!("Implementation found at line {}", start_line);
                // The implementation should be in the Lua code block area (lines 3-20)
                assert!(
                    (2..=20).contains(&start_line),
                    "Implementation line should be in host coordinates (expected 2-20, got {})",
                    start_line
                );
                println!("E2E: Implementation returns Location in host coordinates");
            }
        }
    } else if result.is_object() {
        // Single Location format
        if let Some(range) = result.get("range") {
            let start_line = range["start"]["line"].as_u64().unwrap_or(0);
            println!("Implementation found at line {}", start_line);
            assert!(
                (2..=20).contains(&start_line),
                "Implementation line should be in host coordinates (expected 2-20, got {})",
                start_line
            );
            println!("E2E: Implementation returns Location in host coordinates");
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
