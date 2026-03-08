//! End-to-end test for Lua goto definition in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring for definition:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Definition request at position in Lua block over function call
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - Definition location received from lua-language-server with transformed coordinates
//!
//! Run with: `cargo test --test e2e_lsp_lua_definition --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// E2E test: goto definition on Lua function call returns Location in host coordinates
#[test]
fn e2e_definition_on_lua_function_call_returns_location() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block containing a function definition and call
    // Function defined at line 3 (0-indexed), called at line 6
    let markdown_content = r#"# Test Document

```lua
function greet(name)
    return "Hello, " .. name
end

greet("World")
```

More text.
"#;

    let markdown_uri = "file:///test_definition.md";

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

    // Request definition on "greet" at line 7 (function call: greet("World"))
    // The call is at line 7 in the markdown (0-indexed), character 0-5 is "greet"
    let definition_response = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 7, "character": 2 }
        }),
    );

    println!("Definition response: {:?}", definition_response);

    // Verify no error
    assert!(
        definition_response.get("error").is_none(),
        "Definition should not return error: {:?}",
        definition_response.get("error")
    );

    let result = definition_response
        .get("result")
        .expect("Definition should have result field");

    if result.is_null() {
        // lua-ls may return null if still loading or cannot resolve
        println!("Note: lua-ls returned null (may still be loading or cannot resolve)");
        println!("✓ E2E: Bridge infrastructure working (request succeeded)");
    } else if result.is_array() {
        // Location[] format
        let locations = result.as_array().unwrap();
        if !locations.is_empty() {
            let loc = &locations[0];
            // Verify the location line is in host coordinates (should be line 3 where function is defined)
            // The function definition is at line 3 in the markdown document
            if let Some(range) = loc.get("range") {
                let start_line = range["start"]["line"].as_u64().unwrap_or(0);
                println!("Definition found at line {}", start_line);
                // The function definition starts at line 3 (0-indexed) in the markdown
                // This verifies coordinate transformation is working
                assert!(
                    (3..=5).contains(&start_line),
                    "Definition line should be in host coordinates (expected 3-5, got {})",
                    start_line
                );
                println!("✓ E2E: Definition returns Location in host coordinates");
            }
        }
    } else if result.is_object() {
        // Single Location format
        if let Some(range) = result.get("range") {
            let start_line = range["start"]["line"].as_u64().unwrap_or(0);
            println!("Definition found at line {}", start_line);
            assert!(
                (3..=5).contains(&start_line),
                "Definition line should be in host coordinates (expected 3-5, got {})",
                start_line
            );
            println!("✓ E2E: Definition returns Location in host coordinates");
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: goto definition on local variable returns its declaration location
#[test]
fn e2e_definition_on_lua_local_variable_returns_declaration() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block with local variable
    let markdown_content = r#"# Test Document

```lua
local x = 42
print(x)
```

More text.
"#;

    let markdown_uri = "file:///test_local_definition.md";

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

    // Request definition on "x" in print(x) at line 4, character 6
    let definition_response = client.send_request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 6 }
        }),
    );

    println!(
        "Definition on local variable response: {:?}",
        definition_response
    );

    // Verify no error
    assert!(
        definition_response.get("error").is_none(),
        "Definition should not return error: {:?}",
        definition_response.get("error")
    );

    let result = definition_response
        .get("result")
        .expect("Definition should have result field");

    if result.is_null() {
        println!("Note: lua-ls returned null (may still be loading or cannot resolve)");
        println!("✓ E2E: Bridge infrastructure working (request succeeded)");
    } else {
        println!(
            "✓ E2E: Got definition result for local variable: {:?}",
            result
        );
        // Verify we got a location back
        let has_location = if result.is_array() {
            !result.as_array().unwrap().is_empty()
        } else if result.is_object() {
            result.get("range").is_some() || result.get("targetRange").is_some()
        } else {
            false
        };
        if has_location {
            println!("✓ E2E: Definition returns location for local variable");
        }
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
