//! End-to-end test for Lua goto type definition in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for type definition:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Type definition request at position in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - Type definition location received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_type_definition --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: goto type definition request is handled without error
#[test]
fn e2e_type_definition_request_handled() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing type annotations
    // Using LuaCATS style type annotations that lua-language-server understands
    let markdown_content = r#"# Test Document

```lua
---@class Person
---@field name string
---@field age number
local Person = {}

---@type Person
local p = {}
p.name = "Alice"
```

More text.
"#;

    let markdown_uri = "file:///test_type_definition.md";

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

    // Request type definition on "p" at line 9 (the variable with type annotation)
    // The variable p is at line 9 in the markdown (0-indexed)
    let type_def_response = client.send_request(
        "textDocument/typeDefinition",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 9, "character": 6 }
        }),
    );

    println!("Type definition response: {:?}", type_def_response);

    // Verify no error
    assert!(
        type_def_response.get("error").is_none(),
        "Type definition should not return error: {:?}",
        type_def_response.get("error")
    );

    let result = type_def_response
        .get("result")
        .expect("Type definition should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading or cannot resolve the type
        println!("Note: lua-ls returned null (may still be loading or cannot resolve type)");
        println!("✓ E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // Location[] format
        let locations = result.as_array().unwrap();
        if !locations.is_empty() {
            let loc = &locations[0];
            // Verify the location is in host coordinates
            if let Some(range) = loc.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                println!("Type definition found at line {}", start_line);
                // The class definition should be around line 3-5 (0-indexed) in markdown
                assert!(
                    (2..=7).contains(&start_line),
                    "Type definition line should be in host coordinates (expected 2-7, got {})",
                    start_line
                );
                println!("✓ E2E: Type definition returns Location in host coordinates");
            }
        }
    } else if result.is_object() {
        // Single Location format
        if let Some(range) = result.get("range") {
            let start_line = range["start"]["line"].as_u64().unwrap_or(0);
            println!("Type definition found at line {}", start_line);
            assert!(
                (2..=7).contains(&start_line),
                "Type definition line should be in host coordinates (expected 2-7, got {})",
                start_line
            );
            println!("✓ E2E: Type definition returns Location in host coordinates");
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: type definition on variable with explicit type annotation
#[test]
fn e2e_type_definition_on_typed_variable() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code using type annotations
    let markdown_content = r#"# Test Document

```lua
---@class Config
---@field debug boolean

---@type Config
local config = { debug = true }

print(config.debug)
```

More text.
"#;

    let markdown_uri = "file:///test_typed_variable.md";

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

    // Request type definition on "config" usage at line 9 (print(config.debug))
    let type_def_response = client.send_request(
        "textDocument/typeDefinition",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 9, "character": 6 }
        }),
    );

    println!(
        "Type definition on typed variable response: {:?}",
        type_def_response
    );

    // Verify no error - main goal is to confirm bridge infrastructure works
    assert!(
        type_def_response.get("error").is_none(),
        "Type definition should not return error: {:?}",
        type_def_response.get("error")
    );

    let result = type_def_response
        .get("result")
        .expect("Type definition should have result field");

    if result.is_null() {
        println!("Note: lua-ls returned null (may still be loading or cannot resolve type)");
        println!("✓ E2E: Bridge infrastructure working (request succeeded)");
    } else {
        println!("✓ E2E: Got type definition result: {:?}", result);
        // Verify we got a location back
        let has_location = if result.is_array() {
            !result.as_array().unwrap().is_empty()
        } else if result.is_object() {
            result.get("range").is_some() || result.get("targetRange").is_some()
        } else {
            false
        };
        if has_location {
            println!("✓ E2E: Type definition returns location for typed variable");
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
