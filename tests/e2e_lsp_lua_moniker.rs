//! End-to-end test for Lua moniker in Markdown code blocks via kakehashi binary.
//!
//! This test verifies the full bridge infrastructure wiring:
//! - kakehashi binary spawned via LspClient (not direct BridgeConnection)
//! - Markdown document with Lua code block opened via didOpen
//! - Moniker request at position on symbol in Lua block
//! - kakehashi detects injection, translates position, spawns lua-ls
//! - Response received from lua-language-server (may be null if not supported)
//!
//! Run with: `cargo test --test e2e_lsp_lua_moniker --features e2e`
//!
//! **Requirements**: lua-language-server must be installed and in PATH.
//!
//! **Note**: lua-language-server may not support textDocument/moniker, in which case
//! the test verifies that the bridge infrastructure works (request succeeds without error).

#![cfg(feature = "e2e")]

mod helpers;

use helpers::lua_bridge::{
    create_lua_configured_client, shutdown_client, skip_if_lua_ls_unavailable,
};
use serde_json::json;

/// Helper to check if response has monikers
fn has_monikers(result: &serde_json::Value) -> bool {
    if result.is_null() {
        return false;
    }

    if let Some(arr) = result.as_array() {
        !arr.is_empty()
    } else {
        false
    }
}

/// E2E test: moniker request on local variable in Lua code block
///
/// This test verifies that:
/// 1. The textDocument/moniker request flows through the bridge
/// 2. Position is correctly transformed to virtual coordinates
/// 3. Response is returned without error (even if lua-ls doesn't support moniker)
#[test]
fn e2e_moniker_request_on_lua_variable() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block
    let markdown_content = r#"# Test Document

```lua
local greeting = "hello"
print(greeting)
```

More text.
"#;

    let markdown_uri = "file:///test_moniker.md";

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
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Request moniker on "greeting" variable at line 4, character 6 (on the 'g' of greeting)
    // Line 4 (0-indexed from markdown), character 6 (print(greeting))
    let moniker_response = client.send_request(
        "textDocument/moniker",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 4, "character": 6 }
        }),
    );

    println!("Moniker response: {:?}", moniker_response);

    // Verify no error - this is the key assertion for bridge infrastructure
    assert!(
        moniker_response.get("error").is_none(),
        "Moniker request should not return error: {:?}",
        moniker_response.get("error")
    );

    let result = moniker_response
        .get("result")
        .expect("Moniker response should have result field");

    // Check what we got
    if has_monikers(result) {
        // lua-ls supports moniker and returned results
        println!("E2E: Moniker request returned monikers from lua-language-server");
        if let Some(arr) = result.as_array() {
            for moniker in arr {
                if let Some(scheme) = moniker.get("scheme") {
                    println!("  scheme: {:?}", scheme);
                }
                if let Some(identifier) = moniker.get("identifier") {
                    println!("  identifier: {:?}", identifier);
                }
            }
        }
    } else if result.is_null() {
        // lua-ls may return null if it doesn't support moniker or has no data
        println!("Note: lua-ls returned null (may not support moniker or no data for this symbol)");
        println!("E2E: Bridge infrastructure working (request succeeded without error)");
    } else if result.as_array().is_some_and(|arr| arr.is_empty()) {
        // lua-ls returned empty array - valid response indicating no monikers
        println!("Note: lua-ls returned empty array (no monikers for this symbol)");
        println!("E2E: Bridge infrastructure working (request succeeded)");
    } else {
        println!("E2E: Got unexpected moniker result format: {:?}", result);
    }

    // Clean shutdown
    shutdown_client(&mut client);
}

/// E2E test: moniker request on built-in function in Lua code block
#[test]
fn e2e_moniker_request_on_lua_builtin() {
    if skip_if_lua_ls_unavailable() {
        return;
    }

    let mut client = create_lua_configured_client();

    // Open markdown document with Lua code block calling a built-in
    let markdown_content = r#"# Test Document

```lua
local result = string.format("hello %s", "world")
```

More text.
"#;

    let markdown_uri = "file:///test_moniker_builtin.md";

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
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Request moniker on "string" (line 3, character 15 - on 'string')
    let moniker_response = client.send_request(
        "textDocument/moniker",
        json!({
            "textDocument": { "uri": markdown_uri },
            "position": { "line": 3, "character": 15 }
        }),
    );

    println!("Moniker on built-in response: {:?}", moniker_response);

    // Verify no error - key assertion
    assert!(
        moniker_response.get("error").is_none(),
        "Moniker request should not return error: {:?}",
        moniker_response.get("error")
    );

    let result = moniker_response
        .get("result")
        .expect("Moniker response should have result field");

    if has_monikers(result) {
        println!("E2E: Moniker on built-in returned results");
    } else {
        println!("E2E: Bridge infrastructure working for moniker (no error on built-in)");
    }

    // Clean shutdown
    shutdown_client(&mut client);
}
